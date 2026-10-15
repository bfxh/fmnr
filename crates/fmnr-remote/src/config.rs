//! SSH 端点与凭据的**纯**解析层。
//!
//! 这一层不碰网络，所以能在任何机器上真跑测试。选了「接口 + 无主机自动跳过」
//! 之后，**可验证的部分基本都在这里**：端点写错、凭据落到日志、host key 策略
//! 被默认成「谁都信」——这三类都能在这里钉死，不需要连上真机器。

use std::path::PathBuf;

/// 已解析的 SSH 端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshEndpoint {
    pub user: String,
    pub host: String,
    pub port: u16,
    /// 远端起始路径；空串表示「登录后的默认目录」，由服务端 `realpath(.)` 决定。
    pub path: String,
}

/// SSH 默认端口。
pub const DEFAULT_SSH_PORT: u16 = 22;

/// host key 校验策略。
///
/// **默认必须是 `VerifyKnownHosts`**。`AcceptAny` 之所以单独列出来而不是
/// 图省事直接写死，是因为「先把连接打通再说」是这类接口的头号安全洞：
/// 中间人可以冒充任何服务器，而文件管理器拿到的是全部文件内容。
/// 要用宽松策略只能由调用方显式选择，且 UI 要把风险显示出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostKeyPolicy {
    /// 只信任 `known_hosts` 里已有的指纹；未知主机键**拒绝连接**。
    #[default]
    VerifyKnownHosts,
    /// 信任首次见到的主机键（TOFU）。第二次连同一主机若键变了仍拒绝。
    TrustOnFirstUse,
    /// 不校验。**仅调试用**，任何真实数据都不该走这条路。
    AcceptAny,
}

impl HostKeyPolicy {
    /// 是否属于「不校验」——UI 据此决定要不要挂红色风险提示。
    pub fn is_insecure(self) -> bool {
        matches!(self, HostKeyPolicy::AcceptAny)
    }
}

/// 认证方式。
#[derive(Clone)]
pub enum AuthMethod {
    /// 口令。**故意不实现 Debug**：默认派生会把口令打进日志。
    Password(String),
    /// 私钥文件路径（可带口令）。
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// 交给本机 ssh-agent。
    Agent,
}

/// 手写的 Debug 保证任何一次 `{:?}` 都不会泄露凭据。
impl std::fmt::Debug for AuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthMethod::Password(_) => f.write_str("Password(**redacted**)"),
            AuthMethod::KeyFile { path, passphrase } => f
                .debug_struct("KeyFile")
                .field("path", path)
                // 口令有没有给，比口令本身安全得多，也够诊断用。
                .field("passphrase", &passphrase.as_ref().map(|_| "**redacted**"))
                .finish(),
            AuthMethod::Agent => f.write_str("Agent"),
        }
    }
}

/// 一次 SSH 卷的连接配置。
#[derive(Debug, Clone)]
pub struct SshConfig {
    pub endpoint: SshEndpoint,
    pub auth: AuthMethod,
    pub host_key: HostKeyPolicy,
}

/// 解析失败的原因。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EndpointError {
    #[error("缺少 ssh:// 前缀：{0}")]
    MissingScheme(String),
    #[error("缺少用户名（形如 ssh://user@host/path）：{0}")]
    MissingUser(String),
    #[error("缺少主机名：{0}")]
    MissingHost(String),
    #[error("用户名为空：{0}")]
    EmptyUser(String),
    #[error("端口不是数字或超出 0..=65535：{0}")]
    BadPort(String),
    #[error("端口越界：{0}")]
    PortOutOfRange(u32),
    #[error("IPv6 字面量缺少收尾的 ]：{0}")]
    UnclosedIpv6(String),
    #[error("主机名为空：{0}")]
    EmptyHost(String),
}

/// 解析 `ssh://user@host[:port][/path]`。
///
/// 手写而不用 `Url`：`Url` 会做 percent-decode、小写化主机、并把空 path 规范化成
/// `/`，这几条对 SSH 目标都是错的（`~`、带 `%` 的目录名要原样留给服务端）。
pub fn parse_endpoint(s: &str) -> Result<SshEndpoint, EndpointError> {
    let rest = s
        .strip_prefix("ssh://")
        .ok_or_else(|| EndpointError::MissingScheme(s.to_string()))?;

    // 先切出 path：第一个 '/' 之后全是路径，不参与 host 解析。
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i + 1..].to_string()),
        None => (rest, String::new()),
    };

    // user 与 host 的分界取**最后一个** '@'：用户名本身可以含 '@'。
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (u, h),
        None => return Err(EndpointError::MissingUser(s.to_string())),
    };
    if user.is_empty() {
        return Err(EndpointError::EmptyUser(s.to_string()));
    }

    let (host, port) = split_host_port(hostport, s)?;
    if host.is_empty() {
        return Err(EndpointError::EmptyHost(s.to_string()));
    }

    Ok(SshEndpoint {
        user: user.to_string(),
        host,
        port,
        path,
    })
}

/// 拆 `host:port`，正确处理 IPv6 的 `[::1]:2222`（否则冒号会切错）。
fn split_host_port(hp: &str, whole: &str) -> Result<(String, u16), EndpointError> {
    if let Some(stripped) = hp.strip_prefix('[') {
        let close = stripped
            .find(']')
            .ok_or_else(|| EndpointError::UnclosedIpv6(whole.to_string()))?;
        let host = stripped[..close].to_string();
        let tail = &stripped[close + 1..];
        return match tail {
            "" => Ok((host, DEFAULT_SSH_PORT)),
            t => t
                .strip_prefix(':')
                .map(parse_port)
                .ok_or_else(|| EndpointError::BadPort(whole.to_string()))?
                .map(|p| (host, p)),
        };
    }
    match hp.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), parse_port(p)?)),
        None => Ok((hp.to_string(), DEFAULT_SSH_PORT)),
    }
}

fn parse_port(p: &str) -> Result<u16, EndpointError> {
    let v: u32 = p
        .parse()
        .map_err(|_| EndpointError::BadPort(p.to_string()))?;
    if v == 0 || v > u16::MAX as u32 {
        return Err(EndpointError::PortOutOfRange(v));
    }
    Ok(v as u16)
}

impl SshEndpoint {
    /// 卷 ID：`ssh:host[:port]`。
    ///
    /// **刻意不含 path**——与 WSL 卷同口径：同一台主机的不同目录是同一个卷，
    /// 否则图标覆盖、连接池都会按目录碎片化。
    pub fn volume_id(&self) -> fmnr_core::VolumeId {
        let port = if self.port == DEFAULT_SSH_PORT {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        fmnr_core::VolumeId(format!("ssh:{}{port}", self.host))
    }

    /// 拼回一个可显示的端点串。**不含任何凭据**。
    pub fn to_display_string(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let path = if self.path.is_empty() {
            String::new()
        } else {
            format!("/{}", self.path)
        };
        format!(
            "ssh://{}@{}:{port}{path}",
            self.user,
            host,
            port = self.port
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_endpoint() {
        let e = parse_endpoint("ssh://alice@example.com/srv/data").unwrap();
        assert_eq!(e.user, "alice");
        assert_eq!(e.host, "example.com");
        assert_eq!(e.port, 22, "缺端口应落默认 22");
        assert_eq!(e.path, "srv/data");
    }

    #[test]
    fn parses_explicit_port() {
        let e = parse_endpoint("ssh://bob@10.0.0.5:2222/home/bob").unwrap();
        assert_eq!(e.port, 2222);
        assert_eq!(e.host, "10.0.0.5");
        assert_eq!(e.path, "home/bob");
    }

    #[test]
    fn path_is_optional_and_trailing_slash_does_not_make_a_segment() {
        let e = parse_endpoint("ssh://alice@example.com").unwrap();
        assert_eq!(e.path, "");
        let e2 = parse_endpoint("ssh://alice@example.com/").unwrap();
        assert_eq!(e2.path, "", "根斜杠不该变成一个空路径段");
    }

    /// IPv6 必须能带端口——不特殊处理时 `[::1]:2222` 会在第一个冒号处切错。
    #[test]
    fn parses_ipv6_literal_with_port() {
        let e = parse_endpoint("ssh://root@[fe80::1%25eth0]:2222/data").unwrap();
        assert_eq!(e.host, "fe80::1%25eth0");
        assert_eq!(e.port, 2222);
        assert_eq!(e.path, "data");
    }

    #[test]
    fn parses_ipv6_literal_without_port() {
        let e = parse_endpoint("ssh://root@[::1]/x").unwrap();
        assert_eq!(e.host, "::1");
        assert_eq!(e.port, 22);
    }

    #[test]
    fn rejects_unclosed_ipv6() {
        assert_eq!(
            parse_endpoint("ssh://root@[::1/x"),
            Err(EndpointError::UnclosedIpv6("ssh://root@[::1/x".into()))
        );
    }

    /// 用户名里的 `@` 不能把主机名切错（真实目录/账号里有这种）。
    #[test]
    fn user_containing_at_sign_splits_at_the_last_one() {
        let e = parse_endpoint("ssh://group\\user@corp@example.com/a").unwrap();
        assert_eq!(e.user, "group\\user@corp");
        assert_eq!(e.host, "example.com");
    }

    /// `%` 与 `~` 要原样保留：让 URL 层解码会把 `~` 展开成什么、把 `%20`
    /// 变成空格，都是把本地解释强加给远端。
    #[test]
    fn leaves_percent_and_tilde_untouched() {
        let e = parse_endpoint("ssh://u@h/~me/100%25done").unwrap();
        assert_eq!(e.path, "~me/100%25done");
    }

    #[test]
    fn rejects_missing_scheme_and_pieces() {
        assert!(matches!(
            parse_endpoint("example.com/x"),
            Err(EndpointError::MissingScheme(_))
        ));
        // sftp:// 是另一个协议，不能当 ssh:// 收下。
        assert!(matches!(
            parse_endpoint("sftp://u@h/x"),
            Err(EndpointError::MissingScheme(_))
        ));
        assert!(matches!(
            parse_endpoint("ssh://example.com/x"),
            Err(EndpointError::MissingUser(_))
        ));
        assert!(matches!(
            parse_endpoint("ssh://@example.com/x"),
            Err(EndpointError::EmptyUser(_))
        ));
        assert!(matches!(
            parse_endpoint("ssh://u@"),
            Err(EndpointError::EmptyHost(_))
        ));
    }

    #[test]
    fn rejects_bad_ports() {
        // 端口 0 连不上任何东西，>65535 溢出：都不能静默变成别的行为。
        assert_eq!(
            parse_endpoint("ssh://u@h:0/x"),
            Err(EndpointError::PortOutOfRange(0))
        );
        assert_eq!(
            parse_endpoint("ssh://u@h:70000/x"),
            Err(EndpointError::PortOutOfRange(70000))
        );
        assert!(matches!(
            parse_endpoint("ssh://u@h:notaport/x"),
            Err(EndpointError::BadPort(_))
        ));
    }

    #[test]
    fn volume_id_names_host_only_and_omits_default_port() {
        let e = parse_endpoint("ssh://alice@example.com/srv/a").unwrap();
        assert_eq!(e.volume_id().0, "ssh:example.com");
        let f = parse_endpoint("ssh://alice@example.com:2222/srv/b").unwrap();
        assert_eq!(f.volume_id().0, "ssh:example.com:2222");
        // 同主机同端口不同目录 = 同一个卷
        assert_eq!(
            e.volume_id(),
            parse_endpoint("ssh://x@example.com/y").unwrap().volume_id()
        );
    }

    /// 展示串不能带口令——状态栏/日志/错误消息都会印它。
    #[test]
    fn display_string_carries_no_credentials() {
        let cfg = SshConfig {
            endpoint: parse_endpoint("ssh://alice@example.com:22/srv").unwrap(),
            auth: AuthMethod::Password("hunter2".into()),
            host_key: HostKeyPolicy::default(),
        };
        let shown = cfg.endpoint.to_display_string();
        assert_eq!(shown, "ssh://alice@example.com:22/srv");
        assert!(!shown.contains("hunter2"), "展示串泄了口令：{shown}");
        let dbg = format!("{:?}", cfg);
        assert!(!dbg.contains("hunter2"), "Debug 派生泄了口令：{dbg}");
        assert!(dbg.contains("redacted"), "Debug 要留可诊断的占位：{dbg}");
    }

    #[test]
    fn keyfile_debug_redacts_passphrase_but_keeps_path() {
        let a = AuthMethod::KeyFile {
            path: PathBuf::from("/home/alice/.ssh/id_ed25519"),
            passphrase: Some("s3cret".into()),
        };
        let d = format!("{a:?}");
        assert!(!d.contains("s3cret"), "私钥口令泄了：{d}");
        assert!(d.contains("id_ed25519"), "路径要可见才诊断得了：{d}");
    }

    /// 默认值必须是安全的那个：`HostKeyPolicy::default()` 一旦是 AcceptAny，
    /// 所有忘记显式指定的调用点都静默变成不校验。
    #[test]
    fn host_key_policy_defaults_to_verification() {
        assert_eq!(HostKeyPolicy::default(), HostKeyPolicy::VerifyKnownHosts);
        assert!(!HostKeyPolicy::default().is_insecure());
        assert!(HostKeyPolicy::AcceptAny.is_insecure());
        assert!(
            !HostKeyPolicy::TrustOnFirstUse.is_insecure(),
            "TOFU 仍校验键变化，不算不校验"
        );
    }
}
