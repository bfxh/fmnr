//! 「这台电脑」——本机盘符的枚举。
//!
//! 为什么单独一个模块：界面原先只有"当前挂载的那个卷"，
//! 左侧栏因此只有 home 一棵树，切个盘要在挂载框里绕（`MountSpec::Local` 早就支持
//! 任意本地根，缺的只是入口）。这一层把"有哪些盘"变成可测的纯数据。
//!
//! 两条口径和 `space.rs` 一致：
//! - **读不到就是 `None`，不编 0**——0 会被画成"空盘"，那是假答案；
//! - 标签读不到（未格式化、无媒体、权限）不影响这条盘出现在列表里。

use crate::space::Space;
use std::path::PathBuf;

/// 盘的形状。数值取自 Win32 `GetDriveTypeW` 的返回值（稳定 ABI 常量），
/// 非 Windows 上按挂载点粗判，拿不准就 `Unknown`——**不猜成"本地固定盘"**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    Unknown,
    NoRootDir,
    Removable,
    Fixed,
    Network,
    CdRom,
    RamDisk,
}

impl DriveKind {
    /// 从 `GetDriveTypeW` 的返回值映射。0..=6 之外的值归 `Unknown`。
    ///
    /// ⚠ 这个门控不是装饰：唯一调用方在 `#[cfg(windows)]` 的 `list()` 里，
    /// 只写 `#[cfg(windows)]` 的话，linux 的 **lib 构建**（`--all-targets` 也会单独编它）
    /// 会报 `associated function is never used` 而 `-D warnings` 判红——
    /// 本机是 Windows，这类错**结构上看不见**。加上 `test` 是为了让
    /// `unknown_kinds_stay_unknown` 这条映射测试在 ubuntu job 上也照跑。
    #[cfg(any(windows, test))]
    fn from_u32(v: u32) -> Self {
        match v {
            0 => DriveKind::Unknown,
            1 => DriveKind::NoRootDir,
            2 => DriveKind::Removable,
            3 => DriveKind::Fixed,
            4 => DriveKind::Network,
            5 => DriveKind::CdRom,
            6 => DriveKind::RamDisk,
            _ => DriveKind::Unknown,
        }
    }

    /// 界面角标。网络盘和可移动盘要点出来，否则用户会把临时拔掉的 U 盘
    /// 当成常驻盘往里放东西。
    pub fn glyph(&self) -> &'static str {
        match self {
            DriveKind::Fixed => "💽",
            DriveKind::Removable => "🔌",
            DriveKind::Network => "🌐",
            DriveKind::CdRom => "📀",
            DriveKind::RamDisk => "⚡",
            DriveKind::NoRootDir => "◽",
            DriveKind::Unknown => "❔",
        }
    }
}

/// 一块盘。`root` 一律是**带尾分隔符**的盘根（`C:\` / `/` / `/mnt/x`），
/// 因为 `PathBuf::from("C:").join("x")` 是盘相对路径的坑（见 `lib.rs` 的
/// `root_drive_relative`），少那个尾分隔符就会挂到该盘的"当前目录"上去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    pub root: PathBuf,
    pub label: Option<String>,
    pub kind: DriveKind,
    pub space: Option<Space>,
}

impl Drive {
    /// 一行里给人看的名字：`C: 系统盘` 这种。没标签就只给盘符。
    pub fn display(&self) -> String {
        match &self.label {
            Some(l) if !l.trim().is_empty() => format!("{} {}", self.root.display(), l),
            _ => self.root.display().to_string(),
        }
    }
}

/// 列出本机的盘，按 `root` 排好（顺序要稳：界面按下标渲染，
/// 顺序抖一下用户点到的就是另一块盘）。
#[cfg(windows)]
pub fn list() -> Vec<Drive> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };

    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = PathBuf::from(format!("{letter}:\\"));
        let wide: Vec<u16> = std::ffi::OsStr::new(&root)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let kind = DriveKind::from_u32(unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) });
        // 标签：没格式化/无媒体/权限不够都会失败，失败就当没标签，
        // **不能因此把这块盘从列表里去掉**（用户要看得到它在那儿）。
        let mut name_buf = [0u16; 64];
        let mut fs_buf = [0u16; 32];
        let label = (unsafe {
            GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                Some(&mut name_buf),
                None,
                None,
                None,
                Some(&mut fs_buf),
            )
        })
        .map(|()| {
            String::from_utf16_lossy(&name_buf)
                .trim_end_matches('\0')
                .to_string()
        })
        .ok()
        .filter(|s| !s.is_empty());
        // 复用同一个 `root` 去问容量：再拼一次字符串就会拼出 `C:\\` 那种
        // 双反斜杠路径（Rust 字面量里 `"\\\\"` 是两个反斜杠），
        // 而这里要的正是上面那个带单个尾分隔符的盘根。
        let space = crate::space::of(&root).ok();
        out.push(Drive {
            root,
            label,
            kind,
            space: space.map(|s| Space {
                total: s.total,
                free: s.free,
            }),
        });
    }
    out.sort_by(|a, b| a.root.cmp(&b.root));
    out
}

/// 非 Windows：没有盘符概念。列出 `/` 以及 `/mnt`、`/media` 下的子目录，
/// 够 CI 的 ubuntu job 跑通同一条界面路径（枚举非空、每条 root 真能进）。
#[cfg(not(windows))]
pub fn list() -> Vec<Drive> {
    let mut out = vec![Drive {
        root: PathBuf::from("/"),
        label: None,
        kind: DriveKind::Fixed,
        space: crate::space::of(&PathBuf::from("/")).ok(),
    }];
    for base in ["/mnt", "/media"] {
        let Ok(rd) = std::fs::read_dir(base) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                out.push(Drive {
                    root: p.clone(),
                    label: e.file_name().to_string_lossy().to_string().into(),
                    kind: DriveKind::Unknown,
                    space: crate::space::of(&p).ok(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.root.cmp(&b.root));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 枚举不能是空的（空列表在界面上会被读成"这台机器没有盘"）。
    #[test]
    fn enumeration_is_never_empty() {
        let d = list();
        assert!(!d.is_empty(), "一块盘都没枚举到");
        for x in &d {
            println!(
                "盘 {} kind={:?} space={:?}",
                x.display(),
                x.kind,
                x.space.map(|s| (s.total, s.free))
            );
        }
    }

    /// 每条 root 必须**带尾分隔符且真的能进**。
    /// 少了尾分隔符就是 `E:` 那种盘相对写法——那个坑本轮刚在别处修过，
    /// 这里若退回去，左侧栏点一块盘会列出该盘"当前目录"的内容。
    #[test]
    fn every_root_is_a_traversable_absolute_directory() {
        for d in list() {
            let s = d.root.to_string_lossy().to_string();
            assert!(d.root.is_absolute(), "{s} 不是绝对路径");
            assert!(
                s.ends_with('\\') || s.ends_with('/'),
                "{s} 缺尾分隔符：这是盘相对写法，会跟着该盘的当前目录走"
            );
            assert!(d.root.is_dir(), "{s} 进不去");
        }
    }

    /// 报出来的容量不许自相矛盾：`free > total` 或 `total == 0` 都是编的数
    /// （`space` 模块的立场是"读不到返回 Err 不返回 0"，这里要接住那条承诺）。
    #[test]
    fn reported_capacities_are_self_consistent() {
        for d in list() {
            if let Some(sp) = d.space {
                assert!(sp.total > 0, "{} total=0 会被画成空盘", d.root.display());
                assert!(
                    sp.free <= sp.total,
                    "{} free > total：这个数不能给用户看",
                    d.root.display()
                );
            }
        }
    }

    /// 顺序要稳（按 root 排）。界面按下标渲染，顺序抖 = 点错盘。
    #[test]
    fn order_is_sorted_and_stable() {
        let a: Vec<PathBuf> = list().into_iter().map(|d| d.root).collect();
        let mut b = a.clone();
        b.sort();
        assert_eq!(a, b, "盘列表没按 root 排好");
        assert_eq!(list().len(), a.len(), "两次枚举条数不一样");
    }

    /// `NoRootDir`/`Unknown` 这些拿不准的形状不许被美化成"本地固定盘"。
    #[test]
    fn unknown_kinds_stay_unknown() {
        assert_eq!(DriveKind::from_u32(0), DriveKind::Unknown);
        assert_eq!(DriveKind::from_u32(99), DriveKind::Unknown);
        assert_eq!(DriveKind::from_u32(1), DriveKind::NoRootDir);
        assert_eq!(DriveKind::from_u32(3), DriveKind::Fixed);
        assert_eq!(DriveKind::from_u32(4), DriveKind::Network);
    }

    /// 没标签时显示成盘符本身，不能显示成 `"C: "` 这种带空尾巴的串。
    #[test]
    fn display_without_label_is_just_the_root() {
        let d = Drive {
            root: PathBuf::from("Z:\\"),
            label: Some("   ".into()),
            kind: DriveKind::Network,
            space: None,
        };
        assert_eq!(d.display(), "Z:\\");
        let d2 = Drive {
            label: Some("备份盘".into()),
            ..d
        };
        assert_eq!(d2.display(), "Z:\\ 备份盘");
    }
}
