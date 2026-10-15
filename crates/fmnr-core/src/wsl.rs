//! WSL 感知：识别 `\\wsl$\<distro>\...` UNC 路径，并在 UNC 与 Linux 路径间往返。
//!
//! 这里是**纯路径字符串逻辑**，全部可用单测钉死。
//!
//! **本机已实测**（2026-10-16 复核，DESIGN 附录 A 里「本机 WSL 未启用」那条已过时）：
//! - `wsl -l -v` 列出 `Ubuntu-24.04`、`docker-desktop`（均 Stopped、WSL2）
//! - `\\wsl$\Ubuntu-24.04\home\…` 与 `\\wsl.localhost\Ubuntu-24.04\home\…` 两种 UNC
//!   写法都能直接 `read_dir`，无需任何 shell out——所以 WSL 浏览走 [`VolumeKind::Wsl`]
//!   + 本地 UNC 路径即可，不必另做协议层。
//! - `wslpath` 双向可用（`C:\Windows\notepad.exe` ↔ `/mnt/c/Windows/notepad.exe`）。
//!
//! 仍未实测的只有「WSL 未安装/无发行版」那条错误文案，因此这里不做文案匹配，
//! 可用性判断交由调用方按退出码处理（见 [`parse_distro_list`] 的说明）。

use std::path::Path;

/// Windows 侧访问 WSL 文件的 UNC 前缀。
///
/// 两种写法都合法且都要认：`\\wsl$` 是老写法，`\\wsl.localhost` 是
/// Windows 11 推荐的新写法（`\\wsl$` 仍可用，但脚本里更该用后者）。
pub const WSL_UNC_HOSTS: [&str; 2] = ["wsl$", "wsl.localhost"];

/// 一个已识别的 WSL UNC 路径：哪个 distro、distro 内相对哪。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslUnc {
    pub distro: String,
    /// Linux 侧路径，**绝对**、以 `/` 分隔、无前导 `\\`。例：`home/user/docs`。
    pub linux_rel: String,
}

impl WslUnc {
    /// 拼成 `\\wsl.localhost\<distro>\<linux_rel>`。
    ///
    /// 尾部要转成**反斜杠**：这是给 Windows API 用的 UNC，混着 `/` 的路径
    /// 交给 `CreateFileW` 一类接口并不可靠（早先直接拼 `/` 就是错的）。
    pub fn to_unc(&self) -> String {
        if self.linux_rel.is_empty() {
            format!(r"\\wsl.localhost\{}", self.distro)
        } else {
            let win = self.linux_rel.replace('/', "\\");
            format!(r"\\wsl.localhost\{}\{}", self.distro, win)
        }
    }

    /// Linux 侧的绝对路径（`wslpath` 语义下的结果）。根 distro 视为 `/`。
    pub fn to_linux(&self) -> String {
        if self.linux_rel.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", self.linux_rel)
        }
    }
}

/// 是否为 WSL UNC 路径。大小写不敏感（Windows 路径本就如此）。
///
/// 只判**主机名段**，不猜 distro——`\\wsl.localhost\` 后面是不是 distro
/// 由 [`parse_wsl_unc`] 决定，这里不能因为路径不完整就否认它是 WSL 路径。
pub fn looks_like_wsl_unc(path: &Path) -> bool {
    // 先把分隔符归一化再比：raw string 不能以反斜杠收尾，
    // 写成 `r"\\{h}\\"` 会多出**一对**反斜杠而永远匹配不上（本函数曾栽在这上面）。
    let norm = path.to_string_lossy().to_lowercase().replace('\\', "/");
    WSL_UNC_HOSTS
        .iter()
        .any(|h| norm == format!("//{h}") || norm.starts_with(&format!("//{h}/")))
}

/// 解析 WSL UNC → `(distro, linux_rel)`。
///
/// 非 WSL 路径返回 `None`（不是错误——调用方靠这个决定走不走 WSL 分支）。
/// distro 段缺失时返回 `None`：光有 `\\wsl$` 不带 distro 时无法确定归属，
/// 猜一个 distro 名比拒绝更危险。
pub fn parse_wsl_unc(path: &Path) -> Option<WslUnc> {
    let s = path.to_string_lossy().replace('\\', "/");
    // 前导 `\\` 归一化后是空段，先滤掉再取主机名。
    // （早先写成 while-skip 循环，把主机名本身也吃掉了。）
    let mut parts = s.split('/').filter(|p| !p.is_empty());
    let host = parts.next()?.to_lowercase();
    if !WSL_UNC_HOSTS.contains(&host.as_str()) {
        return None;
    }
    let distro = parts.next()?;
    if distro.is_empty() {
        return None;
    }
    let linux_rel = parts.collect::<Vec<_>>().join("/");
    Some(WslUnc {
        distro: distro.to_string(),
        linux_rel,
    })
}

/// `wslpath -u` 语义：Windows UNC → Linux 路径。
pub fn wslpath_to_linux(path: &Path) -> Option<String> {
    parse_wsl_unc(path).map(|u| u.to_linux())
}

/// `wslpath -w` 语义：distro + Linux 路径 → Windows UNC。
///
/// 输入接受 `/home/u` 或 `home/u`。误传 Windows 路径要拒绝，不硬拼。
///
/// **已知不可表达的输入**：Linux 文件名本身可以含 `\`，而 Windows UNC 里
/// `\` 只能是分隔符——这类名字**在 Windows 侧就没有合法表示**
/// （`\\wsl$\D\a\b` 与含反斜杠的文件名 `a\b` 是同一个字符串）。
/// 所以 [`WslUnc::to_unc`] 对这种名字是有歧义的；这不是我们的转换 bug，
/// Windows 自己访问这类文件也做不到。
pub fn wslpath_to_unc(distro: &str, linux_path: &str) -> Option<WslUnc> {
    if distro.is_empty() {
        return None;
    }
    let trimmed = linux_path.trim();
    if trimmed.is_empty() {
        return Some(WslUnc {
            distro: distro.to_string(),
            linux_rel: String::new(),
        });
    }
    // 只接受 Linux 侧路径；误传 Windows 路径要拒绝而不是硬拼。
    if trimmed.contains(":\\") || trimmed.starts_with("\\\\") {
        return None;
    }
    let rel = trimmed
        .trim_start_matches('/')
        .split('/')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    Some(WslUnc {
        distro: distro.to_string(),
        linux_rel: rel,
    })
}

/// Volume ID：`wsl:<distro>`。与 DESIGN §4.3 的命名一致。
pub fn wsl_volume_id(distro: &str) -> crate::VolumeId {
    crate::VolumeId(format!("wsl:{distro}"))
}

/// 契约测试桩：解析 `wsl -l -q` **stdout** 的 distro 名列表。
///
/// 刻意**不**在这里识别「没有安装发行版」那句提示：那句话本机观测不到
/// （本机装了 WSL），按文案匹配就是猜。可用性判断交给调用方按**退出码**决定
/// （见 [`crate::wsl`] 模块文档与 `fmnr-fs` 的探测点）；这里只保证
/// 「拿到一份正常列表时解析得对」，并滤掉空行。
pub fn parse_distro_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn recognizes_both_unc_hosts() {
        assert!(looks_like_wsl_unc(&PathBuf::from(
            r"\\wsl$\Ubuntu\home\user"
        )));
        assert!(looks_like_wsl_unc(&PathBuf::from(
            r"\\wsl.localhost\Ubuntu\home"
        )));
        // 大小写不敏感
        assert!(looks_like_wsl_unc(&PathBuf::from(
            r"\\WSL.LOCALHOST\Ubuntu"
        )));
        // 光有主机段也算 WSL 路径（distro 未知由 parse 判）
        assert!(looks_like_wsl_unc(&PathBuf::from(r"\\wsl$")));
    }

    #[test]
    fn rejects_non_wsl_paths() {
        assert!(!looks_like_wsl_unc(&PathBuf::from(r"\\server\share")));
        assert!(!looks_like_wsl_unc(&PathBuf::from(r"C:\Users\me")));
        // 前缀相似但不是主机段（`wsl$foo` 不是合法主机）
        assert!(!looks_like_wsl_unc(&PathBuf::from(r"D:\wsl$\fake")));
    }

    #[test]
    fn parse_extracts_distro_and_relative() {
        let u =
            parse_wsl_unc(&PathBuf::from(r"\\wsl.localhost\Debian\home\lbx\proj")).expect("应识别");
        assert_eq!(u.distro, "Debian");
        assert_eq!(u.linux_rel, "home/lbx/proj");
        assert_eq!(u.to_linux(), "/home/lbx/proj");
    }

    #[test]
    fn parse_requires_a_distro_segment() {
        // 只有主机段：没有 distro 可归属，拒绝而不是猜。
        assert!(parse_wsl_unc(&PathBuf::from(r"\\wsl$")).is_none());
        assert!(parse_wsl_unc(&PathBuf::from(r"\\wsl.localhost\")).is_none());
        assert!(parse_wsl_unc(&PathBuf::from(r"\\server\share\x")).is_none());
    }

    #[test]
    fn parse_drops_empty_and_trailing_segments() {
        let u = parse_wsl_unc(&PathBuf::from(r"\\wsl$\Ubuntu\a//b\\c\")).expect("识别");
        assert_eq!(u.linux_rel, "a/b/c", "连续分隔符应折叠");
    }

    #[test]
    fn wslpath_roundtrip_unc_to_linux_to_unc() {
        let win = PathBuf::from(r"\\wsl.localhost\Ubuntu\home\user\docs\notes.md");
        let linux = wslpath_to_linux(&win).expect("转换");
        assert_eq!(linux, "/home/user/docs/notes.md");

        let back = wslpath_to_unc("Ubuntu", &linux).expect("回转");
        assert_eq!(back.linux_rel, "home/user/docs/notes.md");
        // 回转用的是规范主机名（wsl.localhost），不是原来的 wsl$——语义相同。
        assert_eq!(
            back.to_unc(),
            r"\\wsl.localhost\Ubuntu\home\user\docs\notes.md"
        );
    }

    #[test]
    fn wslpath_to_unc_rejects_windows_input() {
        // 误把 Windows 路径喂进来要拒绝：硬拼会得到一个不存在的 Linux 路径。
        assert!(wslpath_to_unc("Ubuntu", r"C:\Users\me").is_none());
        assert!(wslpath_to_unc("Ubuntu", r"\\wsl$\Ubuntu\x").is_none());
        assert!(wslpath_to_unc("", "/home").is_none(), "空 distro 名要拒绝");
    }

    #[test]
    fn distro_root_maps_to_slash() {
        let u = parse_wsl_unc(&PathBuf::from(r"\\wsl$\Ubuntu")).expect("识别");
        assert!(u.linux_rel.is_empty());
        assert_eq!(u.to_linux(), "/");
        assert_eq!(u.to_unc(), r"\\wsl.localhost\Ubuntu");
    }

    #[test]
    fn volume_id_names_the_distro() {
        assert_eq!(wsl_volume_id("Ubuntu").0, "wsl:Ubuntu");
    }

    #[test]
    fn empty_wsl_list_is_not_distro_names() {
        assert!(parse_distro_list("").is_empty());
        assert!(parse_distro_list("   \n\n  ").is_empty());
    }

    #[test]
    fn parses_distro_list_robustly() {
        let out = "Ubuntu\r\ndebian\r\n\r\n  openSUSE-Leap-15  \r\n";
        assert_eq!(
            parse_distro_list(out),
            vec![
                "Ubuntu".to_string(),
                "debian".to_string(),
                "openSUSE-Leap-15".to_string()
            ]
        );
    }

    /// 真机锚定：本机实测的 distro 名是 `Ubuntu-24.04`，**同时含点和连字符**。
    /// 早先按 `Ubuntu` 这种干净名字写用例，点号被当成扩展名一类的问题就漏掉了。
    #[test]
    fn parses_real_distro_name_with_dots_and_hyphens() {
        let p = PathBuf::from(r"\\wsl.localhost\Ubuntu-24.04\home\lbx13\.bashrc");
        let u = parse_wsl_unc(&p).expect("应识别真机 distro 名");
        assert_eq!(u.distro, "Ubuntu-24.04");
        assert_eq!(u.linux_rel, "home/lbx13/.bashrc");
        // 隐藏文件在 Linux 侧仍要看得到（UNC 读得到它，不该被当垃圾丢掉）
        assert!(u.linux_rel.contains(".bashrc"));
    }

    #[test]
    fn docker_desktop_distro_is_recognized() {
        // 本机第二个 distro：`docker-desktop`。它不是给用户浏览的，
        // 但识别层不能因为名字陌生就否认。
        let u = parse_wsl_unc(&PathBuf::from(r"\\wsl$\docker-desktop\")).expect("识别");
        assert_eq!(u.distro, "docker-desktop");
        assert_eq!(u.to_linux(), "/");
    }
}
