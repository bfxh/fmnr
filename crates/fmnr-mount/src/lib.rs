//! 卷发现与挂载：把「用户想挂的东西」变成「注册表里的卷」。
//!
//! **为什么单独一个 crate**：挂载要同时认识本地盘、归档、WSL、SSH 四类实现，
//! 而 fmnr-core 是依赖树的叶子（archive/remote 都依赖它）。
//! 把挂载入口塞进 fmnr-core::volreg 会逼出一个循环依赖，
//! 所以这里作为**上层聚合**存在：core 提供 VolumeRegistry 这个空仓，
//! 本 crate 负责往里放东西。
//!
//! 两类概念要分清，别混：
//!
//! * MountSpec —— **可序列化的意图**（"我想挂 D:/x.zip"、"我想连 ssh://u@h"）。
//!   它是配置，可存盘、可从命令行拼、可以失败而不影响别的卷。
//! * Mounted —— **真实构造出来的卷**。挂载可能失败（文件没了、
//!   归档格式不认、SSH 配置错），失败要如实回传原因，不能静默吞掉。
//!
//! 归档卷是**只读**的（ArchiveVolume::writable() 为 false），
//! 本模块会把它标出来，UI 才能显示「（只读）」。

use fmnr_core::volreg::{VolumeRegistry, VolumeSourceInfo};
use fmnr_core::VolumeKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use fmnr_archive::ArchiveVolume;

/// 一次挂载失败的原因。**不吞错**：调用方要能把它显示给用户。
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("路径不存在：{0}")]
    NotFound(PathBuf),
    #[error("不是可识别的归档（支持 zip/tar/tar.gz/tgz/7z）：{0}")]
    NotAnArchive(PathBuf),
    #[error("归档打不开：{path}：{source}")]
    ArchiveOpen {
        path: PathBuf,
        #[source]
        source: fmnr_core::Error,
    },
    #[error("SSH 端点无法解析：{0}")]
    BadEndpoint(String),
    #[error("本地根目录不存在：{0}")]
    BadLocalRoot(PathBuf),
    #[error("卷发现失败：{0}")]
    DiscoveryFailed(String),
}

/// 用户「想挂什么」的可序列化描述。
///
/// 刻意保持**纯数据**：没有 Arc<dyn Volume>、没有句柄、没有连接。
/// 这样它可以存进配置文件、可以进 MCP 参数、可以在挂载失败后原样重试。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MountSpec {
    /// 本地目录/盘符。
    Local { root: PathBuf },
    /// 归档文件，只读挂载。
    Archive { path: PathBuf },
    /// WSL 发行版的 UNC 根（Windows 侧就是普通可 read_dir 的目录）。
    Wsl { distro: String },
    /// SSH/SFTP 远程卷。endpoint 形如 ssh://user@host[:port][/path]。
    Ssh { endpoint: String },
}

impl MountSpec {
    /// 挂载点的人类可读名，用于状态栏与错误信息。
    pub fn display(&self) -> String {
        match self {
            MountSpec::Local { root } => format!("local:{}", root.display()),
            MountSpec::Archive { path } => format!("archive:{}", path.display()),
            MountSpec::Wsl { distro } => format!("wsl:{distro}"),
            MountSpec::Ssh { endpoint } => endpoint.clone(),
        }
    }

    /// 归档卷只读；其余按各自的 writable() 报。挂载前就能回答的静态判断。
    pub fn expect_writable(&self) -> bool {
        !matches!(self, MountSpec::Archive { .. })
    }
}

/// 挂载结果：成功时带回真实卷，失败时带回原因。
pub struct Mounted {
    pub spec: MountSpec,
    pub volume: Option<Arc<dyn fmnr_core::Volume>>,
    pub error: Option<MountError>,
}

/// 把 MountSpec 变成真实卷。**不做注册**，注册由调用方决定。
pub fn mount(spec: &MountSpec) -> Mounted {
    let (volume, error) = match resolve(spec) {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };
    Mounted {
        spec: spec.clone(),
        volume,
        error,
    }
}

fn resolve(spec: &MountSpec) -> Result<Arc<dyn fmnr_core::Volume>, MountError> {
    match spec {
        MountSpec::Local { root } => {
            if !root.is_dir() {
                return Err(MountError::BadLocalRoot(root.clone()));
            }
            Ok(Arc::new(fmnr_fs::LocalVolume::new(root.clone())))
        }
        MountSpec::Archive { path } => {
            if !path.is_file() {
                return Err(MountError::NotFound(path.clone()));
            }
            if !looks_like_archive(path) {
                return Err(MountError::NotAnArchive(path.clone()));
            }
            let vol =
                ArchiveVolume::open(path.clone()).map_err(|source| MountError::ArchiveOpen {
                    path: path.clone(),
                    source,
                })?;
            Ok(Arc::new(vol))
        }
        MountSpec::Wsl { distro } => {
            // Windows 侧 WSL 的 UNC 根就是普通目录；LocalVolume::new 会
            // 自己认出这是 WSL UNC 并把 kind 报成 Wsl（见 fmnr-fs 的注释）。
            let unc = PathBuf::from(format!("\\\\wsl$\\{}", distro));
            Ok(Arc::new(fmnr_fs::LocalVolume::new(unc)))
        }
        MountSpec::Ssh { endpoint } => {
            let ep = fmnr_remote::config::parse_endpoint(endpoint)
                .map_err(|e| MountError::BadEndpoint(e.to_string()))?;
            let config = fmnr_remote::SshConfig {
                endpoint: ep,
                auth: fmnr_remote::AuthMethod::Agent,
                host_key: fmnr_remote::HostKeyPolicy::VerifyKnownHosts,
            };
            Ok(Arc::new(fmnr_remote::SshVolume::new(config)))
        }
    }
}

/// 按文件名判断像不像归档。
///
/// **不能只看 extension()**：test.tar.gz 的 extension 是 gz，
/// a.tar.bz2 是 bz2——只看最后一段会漏掉这些常见组合。
///
/// 两个口径要同时守住：
/// - **必须带点号边界**。早先写的是 `name.ends_with("zip")`，于是 `我传zip`、
///   `backuptar` 这种名字也会被认成归档（点开→挂载失败→用户看到一句莫名的错）。
///   现在一律比 `.{suffix}`，`tgz`/`7z` 这类单词后缀同样要前面是点。
/// - **只列 fmnr-archive 真能解的**。`tar.bz2` / `tbz2` 原先在表里，而索引器按
///   扩展名分流时看到的是 `bz2` ⇒ 直接 `不支持`，而 CHANGELOG 还写着支持
///   ——那是**声称有、实测没有**，不是功能。要真做得先引 bzip2 解码（C 绑定），
///   本轮按"不占位声明"的规矩先把它从表里撤掉，等有人要再做。
pub fn looks_like_archive(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    const SUFFIXES: &[&str] = &[".zip", ".tar", ".tar.gz", ".tgz", ".7z"];
    SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// 记录每个挂载点的结果，供 UI/CLI 展示「挂了什么、哪些失败了」。
#[derive(Default)]
pub struct MountTable {
    entries: Vec<MountRecord>,
}

pub struct MountRecord {
    pub spec: MountSpec,
    pub volume_id: Option<fmnr_core::VolumeId>,
    pub error: Option<String>,
}

impl MountTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 挂载一批 spec 并把成功的注册进 registry。
    ///
    /// 单个失败**不中断**其余挂载：一个坏 zip 不该让 WSL 也挂不上。
    pub fn mount_all(&mut self, registry: &VolumeRegistry, specs: &[MountSpec]) -> Vec<String> {
        let mut failures = Vec::new();
        for spec in specs {
            let m = mount(spec);
            match (m.volume, m.error) {
                (Some(vol), _) => {
                    let id = registry.add(vol);
                    self.entries.push(MountRecord {
                        spec: spec.clone(),
                        volume_id: Some(id),
                        error: None,
                    });
                }
                (None, Some(err)) => {
                    failures.push(format!("{}：{err}", spec.display()));
                    self.entries.push(MountRecord {
                        spec: spec.clone(),
                        volume_id: None,
                        error: Some(err.to_string()),
                    });
                }
                (None, None) => unreachable!("mount 必须二选一"),
            }
        }
        failures
    }

    pub fn entries(&self) -> &[MountRecord] {
        &self.entries
    }
}

/// 扫描目录下所有可挂载的归档，生成 spec 列表（不挂载）。
pub fn discover_archives(dir: &Path) -> Vec<MountSpec> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<MountSpec> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && looks_like_archive(p))
        .map(|path| MountSpec::Archive { path })
        .collect();
    // 目录枚举顺序不保证稳定；排序让 UI 列表和测试断言都可复现。
    out.sort_by_key(|a| a.display());
    out
}

/// 合成一份「开箱即用」的挂载计划：本地 home + 指定目录下的归档。
///
/// 这是 UI 与 CLI 的**共同起点**，避免两边各自拼一套逻辑而分叉。
pub fn default_plan(home: &Path, archive_dir: Option<&Path>) -> Vec<MountSpec> {
    let mut specs = vec![MountSpec::Local {
        root: home.to_path_buf(),
    }];
    if let Some(dir) = archive_dir {
        specs.extend(discover_archives(dir));
    }
    specs
}

/// 把挂载表转成对外可展示的清单。
pub fn sources_of(table: &MountTable) -> Vec<VolumeSourceInfo> {
    table
        .entries()
        .iter()
        .map(|r| VolumeSourceInfo {
            display: r.spec.display(),
            mounted: r.volume_id.is_some(),
            error: r.error.clone(),
            kind: kind_of(&r.spec),
        })
        .collect()
}

fn kind_of(spec: &MountSpec) -> VolumeKind {
    match spec {
        MountSpec::Local { .. } => VolumeKind::Local,
        MountSpec::Archive { .. } => VolumeKind::Archive,
        MountSpec::Wsl { .. } => VolumeKind::Wsl,
        MountSpec::Ssh { .. } => VolumeKind::Remote,
    }
}

/// 便于调用方统一处理：把 spec 列表一次性挂上并返回挂载表。
pub fn mount_plan(registry: &VolumeRegistry, specs: &[MountSpec]) -> MountTable {
    let mut t = MountTable::new();
    t.mount_all(registry, specs);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_suffix_detection_handles_compound_extensions() {
        // 只看 extension() 会漏掉复合扩展名（test.tar.gz 的 extension 是 gz）。
        assert!(looks_like_archive(Path::new("a.zip")));
        assert!(looks_like_archive(Path::new("a.tar")));
        assert!(looks_like_archive(Path::new("a.tar.gz")));
        assert!(looks_like_archive(Path::new("a.tgz")));
        assert!(looks_like_archive(Path::new("a.7z")));
        // 大小写不敏感
        assert!(looks_like_archive(Path::new("A.ZIP")));
        assert!(looks_like_archive(Path::new("A.7Z")));
        assert!(!looks_like_archive(Path::new("a.txt")));
        assert!(!looks_like_archive(Path::new("a.rs")));
        // 「名字里带 zip」但不是归档后缀，不能误判
        assert!(!looks_like_archive(Path::new("zipnotes.md")));
    }

    /// 🔴 两条一起钉住：
    /// 1. **必须看点号边界**。原先是 `ends_with("zip")` 这类写法，于是 `我做个zip`、
    ///    `backup7z` 都被认成归档 ⇒ 用户点开只得到一句莫名的挂载失败。
    /// 2. **表里不许留打不开的东西**。`tar.bz2`/`tbz2` 原先在这张表里，而
    ///    `fmnr-archive` 按扩展名分流时看到的是 `bz2` ⇒ 挂了必失败；CHANGELOG 却
    ///    把它列为支持格式。现在两边对齐：真能解的才进表（要支持 bz2 得先引
    ///    bzip2 解码，那之前它就是"不支持"，别用检测表假装支持）。
    #[test]
    fn detection_uses_a_dot_boundary_and_only_offers_what_opens() {
        assert!(!looks_like_archive(Path::new("我做个zip")));
        assert!(!looks_like_archive(Path::new("backup7z")));
        assert!(!looks_like_archive(Path::new("mytar")));
        assert!(
            !looks_like_archive(Path::new("a.tar.bz2")),
            "bz2 还留在检测表里，但 fmnr-archive 打不开它"
        );
        assert!(!looks_like_archive(Path::new("a.tbz2")));
        assert!(looks_like_archive(Path::new("a.7z")));
        // 空名字/只有点号也不能崩
        assert!(!looks_like_archive(Path::new("")));
        assert!(!looks_like_archive(Path::new(".")));
    }

    #[test]
    fn archive_spec_is_readonly_others_are_not() {
        assert!(!MountSpec::Archive {
            path: PathBuf::from("a.zip")
        }
        .expect_writable());
        assert!(MountSpec::Local {
            root: PathBuf::from("C:/")
        }
        .expect_writable());
    }

    #[test]
    fn missing_archive_reports_not_found_not_generic_error() {
        let spec = MountSpec::Archive {
            path: PathBuf::from("D:\\definitely\\not\\here.zip"),
        };
        let m = mount(&spec);
        assert!(m.volume.is_none());
        assert!(matches!(m.error, Some(MountError::NotFound(_))));
    }

    #[test]
    fn nonexistent_local_root_fails_loudly() {
        let spec = MountSpec::Local {
            root: PathBuf::from("D:\\no\\such\\dir\\anywhere"),
        };
        let m = mount(&spec);
        assert!(matches!(m.error, Some(MountError::BadLocalRoot(_))));
    }

    #[test]
    fn bad_ssh_endpoint_is_reported_not_panicked() {
        let m = mount(&MountSpec::Ssh {
            endpoint: "not-an-ssh-url".into(),
        });
        assert!(m.volume.is_none());
        assert!(matches!(m.error, Some(MountError::BadEndpoint(_))));
    }

    #[test]
    fn one_bad_spec_does_not_block_the_rest() {
        let reg = VolumeRegistry::new();
        let specs = vec![
            MountSpec::Archive {
                path: PathBuf::from("D:\\nope.zip"),
            },
            MountSpec::Wsl {
                distro: "Ubuntu-24.04".into(),
            },
        ];
        let table = mount_plan(&reg, &specs);
        // 归档挂了（失败），但 WSL 仍然被注册——一个坏输入不该拖垮整批。
        assert_eq!(table.entries().len(), 2);
        assert!(table.entries()[0].volume_id.is_none());
        assert!(table.entries()[1].volume_id.is_some());
        assert_eq!(reg.list().len(), 1);
    }

    #[test]
    fn spec_roundtrips_through_json() {
        // 能序列化才谈得上「存进配置文件 / 从 MCP 参数传来」。
        let spec = MountSpec::Archive {
            path: PathBuf::from("D:\\x\\a.zip"),
        };
        let j = serde_json::to_string(&spec).unwrap();
        let back: MountSpec = serde_json::from_str(&j).unwrap();
        assert_eq!(spec, back);
        assert!(j.contains("\"kind\":\"archive\""));
    }

    #[test]
    fn sources_surface_failures() {
        let reg = VolumeRegistry::new();
        let table = mount_plan(
            &reg,
            &[MountSpec::Archive {
                path: PathBuf::from("D:\\nope.zip"),
            }],
        );
        let srcs = sources_of(&table);
        assert_eq!(srcs.len(), 1);
        assert!(!srcs[0].mounted);
        assert!(srcs[0].error.is_some(), "失败必须对外可见，不能静默");
    }

    #[test]
    fn wsl_spec_declares_wsl_kind() {
        // 挂载前的静态判断：WSL spec 不该被当成普通本地卷。
        assert_eq!(
            kind_of(&MountSpec::Wsl { distro: "X".into() }),
            VolumeKind::Wsl
        );
        assert_eq!(
            kind_of(&MountSpec::Ssh {
                endpoint: "ssh://u@h".into()
            }),
            VolumeKind::Remote
        );
    }
}
/// 枚举本机 WSL 发行版（`wsl -l -q`）。
///
/// **发现入口**：UI 的挂载对话框靠它列出可选 distro，
/// 用户不用手打发行版名（打错一个字母就是个指向不存在 UNC 的卷）。
///
/// 没装 WSL / 命令失败时返回 Err，调用方**据错误提示**而不是硬编码文案；
/// 这里刻意不匹配 stderr 文案——那句话在装了 WSL 的机器上观测不到，
/// 按文案匹配等于猜（同 `fmnr_core::wsl` 模块的既定口径，按退出码判）。
pub fn discover_wsl_distros() -> Result<Vec<String>, MountError> {
    let out = std::process::Command::new("wsl")
        .args(["-l", "-q"])
        .output()
        .map_err(|e| MountError::DiscoveryFailed(format!("无法执行 wsl.exe：{e}")))?;
    if !out.status.success() {
        return Err(MountError::DiscoveryFailed(
            "wsl -l -q 退出码非 0（未安装 WSL？）".to_string(),
        ));
    }
    // `wsl` 的输出是 UTF-16LE（实测），直接 from_utf8 会得到乱码。
    let text = decode_utf16le(&out.stdout)
        .ok_or_else(|| MountError::DiscoveryFailed("wsl 输出不是合法 UTF-16LE".into()))?;
    let list = fmnr_core::wsl::parse_distro_list(&text);
    if list.is_empty() {
        return Err(MountError::DiscoveryFailed(
            "wsl -l -q 返回了空列表（没有任何发行版）".to_string(),
        ));
    }
    Ok(list)
}

/// UTF-16LE → String，容忍结尾缺字节与 BOM。
///
/// 与 `fmnr-fs/tests/wsl_live.rs` 的实测结论一致：wsl.exe 的 stdout
/// 是 UTF-16LE 而非 UTF-8。测试里的副本将来可以收敛到这。
fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    let body = if bytes.starts_with(&[0xff, 0xfe]) {
        &bytes[2..]
    } else {
        bytes
    };
    if body.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn decode_utf16le_handles_bom_and_no_bom() {
        let hello: Vec<u8> = vec![0xff, 0xfe, b'H', 0, b'i', 0];
        assert_eq!(decode_utf16le(&hello).as_deref(), Some("Hi"));
        let no_bom: Vec<u8> = vec![b'A', 0, b'B', 0];
        assert_eq!(decode_utf16le(&no_bom).as_deref(), Some("AB"));
        // 奇数长度不是合法 UTF-16 序列，必须 None 而不是 panic。
        assert_eq!(decode_utf16le(&[0x41, 0x00, 0x42]), None);
    }

    #[test]
    fn discover_wsl_distros_live_or_reports_cleanly() {
        // 有 WSL 的机器必须真拿到列表；没有的机器不许红——
        // 返回的必须是 Err（可解释）而不是 Ok 空表（会让 UI 显示"没有发行版"
        // 而用户其实只是没装 WSL）。本机（装了 WSL2）走 if 分支。
        match discover_wsl_distros() {
            Ok(list) => {
                assert!(!list.is_empty(), "Ok 就该非空：空表会误导 UI");
                for d in &list {
                    assert!(!d.trim().is_empty(), "distro 名不该是空白：{d:?}");
                }
            }
            Err(e) => {
                eprintln!("本机无 WSL，跳过：{e}");
                assert!(matches!(e, MountError::DiscoveryFailed(_)));
            }
        }
    }
}
