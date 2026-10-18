//! `SshVolume`：SSH/SFTP 远程卷。
//!
//! 🔴 见 `lib.rs` 的实测声明——**连接路径未经真实主机验证**。
//! 本模块里可验证的部分是 host key 决策（`accept_host_key`，纯函数）；
//! 其余（握手、认证、流式读、断连恢复）只有编译期与类型层的正确性。

use crate::config::{AuthMethod, HostKeyPolicy, SshConfig};
use crate::status::map_status;
use fmnr_core::{
    Attr, ConflictPolicy, Entry, Error, ExternalOpen, ExternalOpens, IconKey, ListOpts, OpReport,
    Progress, Result, SortOrder, VPath, Volume, VolumeId, VolumeKind,
};
use russh::client;
use russh_sftp::client::SftpSession;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 已知主机键的存放处。
///
/// 做成 trait 而不是直接读 `~/.ssh/known_hosts`，是因为**没有**在这里重新实现
/// 一份 OpenSSH 的 known_hosts 语法（hashed 条目、`@cert-authority`、多行模式）。
/// 自己糊一个解析器会比不解析更危险：解析错的条目会被当成「已验证过」。
/// 接真实 store 时应当用现成实现或明确列出未覆盖的语法。
pub trait KnownHosts: Send + Sync {
    /// 返回该 host:port 已记录的公钥（可能多个算法）。未知主机返回空。
    fn keys_for(&self, host: &str, port: u16) -> Vec<String>;
    /// TOFU 用：记录首次见到的键。
    fn remember(&self, host: &str, port: u16, key: &str);
}

/// 空的 store：所有主机都算「未知」。
///
/// 这是默认值，配合 `VerifyKnownHosts` 得到**fail-closed**——
/// 第一次连任何主机都会被拒。宁可让人显式配置，不要静默信任。
#[derive(Debug, Default)]
pub struct EmptyKnownHosts;

impl KnownHosts for EmptyKnownHosts {
    fn keys_for(&self, _host: &str, _port: u16) -> Vec<String> {
        Vec::new()
    }
    fn remember(&self, _host: &str, _port: u16, _key: &str) {}
}

/// host key 决策的**纯**函数——安全性的真正落点，可在无网络下穷尽测试。
///
/// `presented` 是服务端本次出示的凭据，`stored` 是该主机已记录的键集合。
/// 服务端可能出示**普通公钥**或**证书**，两者不能混为一谈：
/// 把证书字符串拿去和 known_hosts 里的裸公钥比，要么永远不匹配（误拒），
/// 要么凑巧匹配上而跳过了 CA 签名校验（误放）。这里没有实现 CA 信任链，
/// 所以 `Certificate` **只在显式 AcceptAny 下放行**——失败要朝安全的一侧倒。
pub fn accept_host_key(policy: HostKeyPolicy, stored: &[String], presented: ServerId<'_>) -> bool {
    match presented {
        ServerId::Certificate => matches!(policy, HostKeyPolicy::AcceptAny),
        ServerId::Key(presented) => match policy {
            // 不校验：任何键都收。仅调试；UI 必须显式标风险。
            HostKeyPolicy::AcceptAny => true,
            HostKeyPolicy::VerifyKnownHosts => {
                // 已知主机：必须命中已记录的键。**未知主机（stored 为空）一律拒绝**，
                // 这是与 TOFU 的关键区别——不因为「没见过」就放行。
                stored.iter().any(|k| k == presented)
            }
            HostKeyPolicy::TrustOnFirstUse => {
                if stored.is_empty() {
                    // 首次见面：放行（调用方随后要 remember，否则下次还是空）。
                    true
                } else {
                    // 已有记录：键变了就是**拒绝**（MITM 或服务器重装，两种都要人看一眼）。
                    stored.iter().any(|k| k == presented)
                }
            }
        },
    }
}

/// 服务端在 key exchange 里出示的凭据种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerId<'a> {
    /// 裸公钥（指纹可比对 known_hosts）。
    Key(&'a str),
    /// X.509/SSH 证书：要验 CA 链，本实现不做。
    Certificate,
}

/// russh 的 Handler：只做 host key 决策，其余走默认（默认是 fail-closed 的）。
struct FmnrHandler {
    policy: HostKeyPolicy,
    stored: Vec<String>,
}

impl client::Handler for FmnrHandler {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> impl std::future::Future<Output = std::result::Result<bool, Self::Error>> + Send {
        let policy = self.policy;
        let stored = self.stored.clone();
        // 先把 enum 收成 owned 文本，避免在 async block 里借引用。
        let presented = match server_public_key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => {
                Presented::Key(key.to_string())
            }
            russh::keys::PublicKeyOrCertificate::Certificate(_) => Presented::Certificate,
        };
        async move {
            let ok = match &presented {
                Presented::Key(s) => accept_host_key(policy, &stored, ServerId::Key(s)),
                Presented::Certificate => accept_host_key(policy, &[], ServerId::Certificate),
            };
            if !ok {
                tracing::warn!(
                    policy = ?policy,
                    "拒绝服务端主机凭据（未知主机或键不匹配；证书未实现 CA 校验）"
                );
            }
            Ok(ok)
        }
    }
}

enum Presented {
    Key(String),
    Certificate,
}

/// SSH/SFTP 卷。
pub struct SshVolume {
    config: SshConfig,
    id: VolumeId,
    external_opens: Arc<ExternalOpens>,
    known_hosts: Arc<dyn KnownHosts>,
}

impl SshVolume {
    pub fn new(config: SshConfig) -> Self {
        let id = config.endpoint.volume_id();
        Self {
            config,
            id,
            external_opens: Arc::new(ExternalOpens::new()),
            known_hosts: Arc::new(EmptyKnownHosts),
        }
    }

    pub fn with_known_hosts(mut self, store: Arc<dyn KnownHosts>) -> Self {
        self.known_hosts = store;
        self
    }

    /// 建立连接并开一个 SFTP 会话。
    ///
    /// 每次调用都新建连接：连接复用要处理多路复用、空闲失效、重连竞态，
    /// 而**这些今天没有真实主机可测**。先选「简单且不会错」的做法，
    /// 性能等有了实测环境再优化——否则是在猜。
    async fn session(&self) -> Result<Arc<SftpSession>> {
        let host = self.config.endpoint.host.clone();
        let port = self.config.endpoint.port;
        let addr = (host.as_str(), port);
        let stored = self.known_hosts.keys_for(&host, port);
        let handler = FmnrHandler {
            policy: self.config.host_key,
            stored,
        };

        let mut handle = client::connect(Arc::new(client::Config::default()), addr, handler)
            .await
            .map_err(|e| Error::Other(format!("连接 {host}:{port} 失败：{e}")))?;

        self.authenticate(&mut handle).await?;

        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| Error::Other(format!("开会话通道失败：{e}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| Error::Other(format!("开 sftp 子系统失败：{e}")))?;

        let stream = channel.into_stream();
        let session = SftpSession::new(stream)
            .await
            .map_err(|e| Error::Other(format!("sftp 握手失败：{e}")))?;
        Ok(Arc::new(session))
    }

    async fn authenticate(&self, handle: &mut client::Handle<FmnrHandler>) -> Result<()> {
        let user = self.config.endpoint.user.as_str();
        let result = match &self.config.auth {
            AuthMethod::Password(p) => handle
                .authenticate_password(user, p)
                .await
                .map_err(|e| Error::Other(format!("口令认证请求失败：{e}")))?,
            AuthMethod::KeyFile { path, passphrase } => {
                let key = russh::keys::load_secret_key(path, passphrase.as_deref())
                    .map_err(|e| Error::Other(format!("读私钥失败：{e}")))?;
                // `authenticate_publickey` 要的是 `PrivateKeyWithHashAlg`，
                // 裸 `PrivateKey` 不行：RSA 的签名哈希算法得在这一步定下来。
                let signed = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None);
                handle
                    .authenticate_publickey(user, signed)
                    .await
                    .map_err(|e| Error::Other(format!("公钥认证请求失败：{e}")))?
            }
            AuthMethod::Agent => {
                return Err(Error::Unsupported(
                    "ssh-agent 认证尚未实现（需接 agent 协议，且无主机可测）".into(),
                ))
            }
        };
        // 认证「请求发出去了」和「认证通过了」是两件事：只查请求不报错的话，
        // 服务端拒绝时我们会当成已登录继续往下走。
        match result {
            russh::client::AuthResult::Success => Ok(()),
            russh::client::AuthResult::Failure {
                remaining_methods,
                partial_success,
            } => Err(Error::Other(format!(
                "认证被拒（用户 {user}，partial_success={partial_success}，服务端剩余方式：{remaining_methods:?}）"
            ))),
        }
    }

    /// 远端绝对路径：卷内路径直接当作服务端路径。
    fn remote(&self, path: &VPath) -> String {
        let raw = path.0.to_string_lossy().replace('\\', "/");
        let stripped = raw
            .strip_prefix("ssh:")
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or(raw.as_str());
        if stripped.is_empty() {
            // 空路径要问服务端「我在哪」，不能硬编 "/"：
            // 那会把用户从 home 突然挪到根，看起来像目录凭空变了。
            ".".to_string()
        } else {
            format!("/{stripped}")
        }
    }
}

#[async_trait::async_trait]
impl Volume for SshVolume {
    fn id(&self) -> VolumeId {
        self.id.clone()
    }

    fn kind(&self) -> VolumeKind {
        VolumeKind::Remote
    }

    async fn list(&self, path: &VPath, opts: &ListOpts) -> Result<Vec<Entry>> {
        let session = self.session().await?;
        let dir = self.remote(path);
        let mut entries = Vec::new();
        // read_dir 返回含 "."/".." 的迭代，要滤掉——否则 UI 里会出现
        // 两个点开的目录名和「自己」。
        for de in session
            .read_dir(&dir)
            .await
            .map_err(|e| sftp_err(e, &dir))?
        {
            let name = de.file_name();
            if name == "." || name == ".." {
                continue;
            }
            // `Metadata` 就是 `FileAttributes`：这些是**字段**不是方法，
            // 且全是 Option——服务端不给 size/mtime 是合规行为，不能当成 0。
            let meta = de.metadata();
            let ft = de.file_type();
            let is_dir = ft.is_dir();
            // 先**标注**，再按显示开关筛——这两件事分开是本地卷上用真跑换来的：
            // 把标注挂在显示开关的分支里，勾上"显示隐藏项"会让标记整个变空。
            let attrs = remote_attrs(&name, ft.is_symlink(), meta.permissions);
            if !opts.show_hidden && attrs.contains(&Attr::Hidden) {
                continue;
            }
            entries.push(Entry {
                name: name.clone(),
                is_dir,
                size: if is_dir { None } else { meta.size },
                modified: meta
                    .mtime
                    .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s as u64)),
                // 远程卷**不做 hash 覆盖**（DESIGN §4.4：远程取 hash 代价太高），
                // 所以这里只给扩展名/通用两档。
                icon_key: if is_dir {
                    IconKey::Directory
                } else {
                    match Path::new(&name)
                        .extension()
                        .map(|e| e.to_string_lossy().to_lowercase())
                    {
                        Some(ext) if !ext.is_empty() => IconKey::ByExtension(ext),
                        _ => IconKey::Generic,
                    }
                },
                attrs,
            });
        }
        sort_entries(&mut entries, opts.sort);
        Ok(entries)
    }

    async fn read_stream(
        &self,
        path: &VPath,
    ) -> Result<Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>> {
        let session = self.session().await?;
        let p = self.remote(path);
        let bytes = session.read(&p).await.map_err(|e| sftp_err(e, &p))?;
        let stream = futures::stream::iter(std::iter::once(Ok(bytes)));
        Ok(Box::new(stream)
            as Box<
                dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin,
            >)
    }

    async fn write(&self, path: &VPath, data: &[u8]) -> Result<()> {
        let session = self.session().await?;
        let p = self.remote(path);
        session.write(&p, data).await.map_err(|e| sftp_err(e, &p))
    }

    fn writable(&self) -> bool {
        true
    }

    fn native_path(&self, _path: &VPath) -> Option<PathBuf> {
        // 远端文件没有本地路径。返回 Some 会让上层把不存在的路径交给系统。
        None
    }

    fn external_opens(&self) -> &ExternalOpens {
        &self.external_opens
    }

    async fn spawn_external_open(&self, file_path: &VPath, _program: &str) -> Result<ExternalOpen> {
        Err(Error::Unsupported(format!(
            "远程文件要先取回本地才能交给外部程序打开：{}",
            file_path.0.display()
        )))
    }

    async fn copy(
        &self,
        _sources: &[VPath],
        _dest_dir: &VPath,
        _policy: ConflictPolicy,
        _progress: Arc<Progress>,
    ) -> Result<OpReport> {
        Err(Error::Unsupported(
            "远程卷的拷贝/移动未实现（服务端移动要覆盖跨目录、冲突策略、进度取消，且今天无主机可测）"
                .into(),
        ))
    }

    async fn move_entries(
        &self,
        _sources: &[VPath],
        _dest_dir: &VPath,
        _policy: ConflictPolicy,
        _progress: Arc<Progress>,
    ) -> Result<OpReport> {
        Err(Error::Unsupported("远程卷的移动未实现".into()))
    }

    async fn delete(&self, targets: &[VPath], _progress: Arc<Progress>) -> Result<OpReport> {
        let session = self.session().await?;
        let mut done = 0usize;
        for t in targets {
            let p = self.remote(t);
            // 先问是不是目录：remove_file 对目录会失败，报错信息会误导。
            match session.metadata(&p).await {
                Ok(m) if m.is_dir() => session.remove_dir(&p).await.map_err(|e| sftp_err(e, &p))?,
                Ok(_) => session.remove_file(&p).await.map_err(|e| sftp_err(e, &p))?,
                Err(e) => return Err(sftp_err(e, &p)),
            }
            done += 1;
        }
        Ok(OpReport {
            done,
            skipped: 0,
            aborted: 0,
            bytes: 0,
        })
    }

    async fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        let session = self.session().await?;
        let (a, b) = (self.remote(from), self.remote(to));
        session.rename(&a, &b).await.map_err(|e| sftp_err(e, &a))
    }

    async fn create_dir(&self, path: &VPath) -> Result<()> {
        let session = self.session().await?;
        let p = self.remote(path);
        session.create_dir(&p).await.map_err(|e| sftp_err(e, &p))
    }

    async fn search(&self, _root: &VPath, _needle: String, _limit: usize) -> Result<Vec<VPath>> {
        // 远程全盘遍历会把整棵树拉一遍，代价与本地完全不同；
        // 没实测就不知道慢到什么程度，所以宁可显式不支持也不静默变慢。
        Err(Error::Unsupported(
            "远程卷的遍历搜索未实现（代价高且无主机可测）".into(),
        ))
    }
}

/// 把 sftp 错误折成卷错误。
/// 远程条目的属性标记。
///
/// 抽成纯函数和 `accept_host_key` 同一个理由：这一支没有可连的真实主机
/// （见 `lib.rs` 的实测声明），能钉住的只有判定本身，不是握手。
///
/// POSIX 没有独立的隐藏位——**点开头就是约定**，所以 `.bashrc` 要标成 Hidden，
/// 和 `\\wsl$` / 归档卷那边"勾上显示就能看到标记"的口径一致。
fn remote_attrs(name: &str, is_symlink: bool, perms: Option<u32>) -> Vec<Attr> {
    let mut attrs = Vec::new();
    if is_symlink {
        attrs.push(Attr::Symlink);
    }
    if name.starts_with('.') {
        attrs.push(Attr::Hidden);
    }
    if let Some(p) = perms {
        // POSIX owner-writable 位为 0 视为只读（是写位 0o200，不是读位 0o400）。
        if p & 0o200 == 0 {
            attrs.push(Attr::ReadOnly);
        }
    }
    attrs
}

fn sftp_err(e: russh_sftp::client::error::Error, path: &str) -> Error {
    use russh_sftp::client::error::Error as Sftp;
    match e {
        Sftp::Status(st) => map_status(st.status_code, path, &st.error_message),
        // Timeout / IO / Limited / Unexpected* 都不是「路径不存在」：
        // 折成 NotFound 会让上层以为文件真的不在，而不是该重试。
        other => Error::Other(format!("{other}（{path}）")),
    }
}

fn sort_entries(entries: &mut [Entry], order: SortOrder) {
    match order {
        SortOrder::Name => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        SortOrder::Size => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)))
        }),
        SortOrder::Modified => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| b.modified.cmp(&a.modified))
        }),
        SortOrder::Type => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.extension().cmp(&b.extension()))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> String {
        s.to_string()
    }

    fn key(s: &str) -> ServerId<'_> {
        ServerId::Key(s)
    }

    /// fail-closed：默认策略下，未知主机（stored 空）**必须**拒。
    /// 这条是整个模块最重要的断言——它一旦被改成放行，中间人就赢了。
    #[test]
    fn verify_policy_rejects_unknown_host() {
        assert!(
            !accept_host_key(
                HostKeyPolicy::VerifyKnownHosts,
                &[],
                key("ssh-ed25519 AAAAattacker")
            ),
            "未知主机被放行了：默认策略失效"
        );
    }

    #[test]
    fn verify_policy_accepts_only_recorded_key() {
        let stored = vec![k("ssh-ed25519 AAAAgood")];
        assert!(accept_host_key(
            HostKeyPolicy::VerifyKnownHosts,
            &stored,
            key("ssh-ed25519 AAAAgood")
        ));
        assert!(
            !accept_host_key(
                HostKeyPolicy::VerifyKnownHosts,
                &stored,
                key("ssh-rsa AAAAother")
            ),
            "键变了还放行就是 MITM 洞"
        );
    }

    /// TOFU：第一次放行，之后**键变了要拒**——这才是 TOFU 的全部意义。
    #[test]
    fn tofu_trusts_first_then_pins() {
        assert!(accept_host_key(
            HostKeyPolicy::TrustOnFirstUse,
            &[],
            key("ssh-ed25519 AAAAfirst")
        ));
        let stored = vec![k("ssh-ed25519 AAAAfirst")];
        assert!(accept_host_key(
            HostKeyPolicy::TrustOnFirstUse,
            &stored,
            key("ssh-ed25519 AAAAfirst")
        ));
        assert!(
            !accept_host_key(
                HostKeyPolicy::TrustOnFirstUse,
                &stored,
                key("ssh-ed25519 AAAAevil")
            ),
            "TOFU 在键变更后必须拒绝"
        );
    }

    #[test]
    fn accept_any_is_the_only_unchecked_mode() {
        assert!(accept_host_key(
            HostKeyPolicy::AcceptAny,
            &[],
            key("anything")
        ));
        assert!(accept_host_key(
            HostKeyPolicy::AcceptAny,
            &[k("other")],
            key("anything")
        ));
        assert!(HostKeyPolicy::AcceptAny.is_insecure());
        assert!(!HostKeyPolicy::VerifyKnownHosts.is_insecure());
    }

    /// 证书要验 CA 链，本实现没做 ⇒ 除了显式 AcceptAny 之外一律拒。
    /// 若哪天有人图省事把证书也走「比对 stored」，这条会红。
    #[test]
    fn certificates_are_rejected_without_ca_validation() {
        for p in [
            HostKeyPolicy::VerifyKnownHosts,
            HostKeyPolicy::TrustOnFirstUse,
        ] {
            assert!(
                !accept_host_key(p, &[], ServerId::Certificate),
                "{p:?} 不该放行证书"
            );
            assert!(
                !accept_host_key(p, &[k("ssh-ed25519 AAAAgood")], ServerId::Certificate),
                "{p:?} 不该把证书和裸公钥混比后放行"
            );
        }
        assert!(
            accept_host_key(HostKeyPolicy::AcceptAny, &[], ServerId::Certificate),
            "AcceptAny 是唯一放行证书的路径（它本来就什么都不验）"
        );
    }

    /// 默认 store 是空的，配默认策略 => 任何真实主机第一次都连不上。
    /// 这是刻意的（见 `EmptyKnownHosts`），此测试钉住它别被「顺手改成 TOFU」。
    #[test]
    fn default_store_and_policy_are_fail_closed() {
        let store = EmptyKnownHosts;
        let keys = store.keys_for("example.com", 22);
        assert!(keys.is_empty());
        assert!(!accept_host_key(
            HostKeyPolicy::default(),
            &keys,
            key("whatever")
        ));
    }

    #[test]
    fn remote_path_empty_asks_server_for_cwd() {
        let vol = SshVolume::new(SshConfig {
            endpoint: crate::config::parse_endpoint("ssh://u@example.com").unwrap(),
            auth: AuthMethod::Agent,
            host_key: HostKeyPolicy::default(),
        });
        // 空路径不能变 "/"：那会把用户从 home 挪到根。
        assert_eq!(vol.remote(&VPath(PathBuf::new())), ".");
        assert_eq!(
            vol.remote(&VPath(PathBuf::from("srv/data"))),
            "/srv/data",
            "相对卷内路径要落到绝对远端路径"
        );
    }

    #[test]
    fn remote_path_strips_scheme_and_backslashes() {
        let vol = SshVolume::new(SshConfig {
            endpoint: crate::config::parse_endpoint("ssh://u@example.com").unwrap(),
            auth: AuthMethod::Agent,
            host_key: HostKeyPolicy::default(),
        });
        assert_eq!(
            vol.remote(&VPath(PathBuf::from(r"ssh:\srv\data"))),
            "/srv/data"
        );
    }

    #[test]
    fn volume_is_remote_and_unified_by_host() {
        let a = SshVolume::new(SshConfig {
            endpoint: crate::config::parse_endpoint("ssh://u@h/srv/a").unwrap(),
            auth: AuthMethod::Agent,
            host_key: HostKeyPolicy::default(),
        });
        let b = SshVolume::new(SshConfig {
            endpoint: crate::config::parse_endpoint("ssh://u@h/srv/b").unwrap(),
            auth: AuthMethod::Password("p".into()),
            host_key: HostKeyPolicy::default(),
        });
        assert_eq!(a.kind(), VolumeKind::Remote);
        assert_eq!(a.id(), b.id(), "同主机不同目录要是同一个卷");
        assert!(a.writable());
        assert!(a.native_path(&VPath(PathBuf::from("x"))).is_none());
    }

    /// 未实现的能力要**明确报 Unsupported**，不能静默成功或返回空列表。
    /// 返回空列表会让客户端以为「远端确实没有」，那是数据丢失级的误导。
    #[tokio::test]
    async fn unimplemented_capabilities_say_so() {
        let vol = SshVolume::new(SshConfig {
            endpoint: crate::config::parse_endpoint("ssh://u@h").unwrap(),
            auth: AuthMethod::Agent,
            host_key: HostKeyPolicy::default(),
        });
        let e = vol
            .search(&VPath(PathBuf::new()), "x".into(), 10)
            .await
            .unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(_)),
            "搜索要 Unsupported：{e}"
        );

        let e = vol
            .copy(
                &[],
                &VPath(PathBuf::new()),
                ConflictPolicy::Skip,
                Arc::new(Progress::new()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(_)),
            "拷贝要 Unsupported：{e}"
        );

        let e = vol
            .spawn_external_open(&VPath(PathBuf::from("x")), "notepad")
            .await
            .unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(_)),
            "远程不能直接外置打开：{e}"
        );
    }

    /// POSIX 约定：点开头就是隐藏。
    ///
    /// 这条锁的是实测发现的缺口——远程卷整个模块从没写过 `Attr::Hidden`，
    /// 只在显示开关那一支按名字 `continue`。形状和本地卷 `1561693` 修掉的
    /// 那条一模一样：**标注**与**筛选**是两件事，混做会让"显示隐藏项"
    /// 打开后标记反而变空。
    #[test]
    fn dotfile_carries_hidden_while_write_bit_drives_readonly() {
        let a = remote_attrs(".bashrc", false, Some(0o644));
        assert!(a.contains(&Attr::Hidden), "点文件没标隐藏：{a:?}");
        assert!(
            !a.contains(&Attr::ReadOnly),
            "0o644 属主可写，不该判只读：{a:?}"
        );

        // 只读看的是**写位** 0o200，不是读位 0o400。
        let b = remote_attrs("notes", false, Some(0o400));
        assert!(
            b.contains(&Attr::ReadOnly),
            "0o400 没有写位，该判只读：{b:?}"
        );
        assert!(!b.contains(&Attr::Hidden), "不带点的名字不该标隐藏");

        // 服务端不给权限位是合规行为：这时不能猜只读。
        let c = remote_attrs("link", true, None);
        assert!(c.contains(&Attr::Symlink), "软链要标出来：{c:?}");
        assert!(
            !c.contains(&Attr::ReadOnly),
            "没有权限位时不该判只读：{c:?}"
        );

        let d = remote_attrs(".profile", false, Some(0o444));
        assert!(
            d.contains(&Attr::Hidden) && d.contains(&Attr::ReadOnly),
            "隐藏 + 只读要同时成立：{d:?}"
        );
    }
}
