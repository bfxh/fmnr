//! 卷的容量与剩余空间。
//!
//! 放在 `fmnr-fs` 而不是 UI 里：这是文件系统的事，而且 Windows 那侧要开
//! `Win32_Storage_FileSystem` 特性——按 target 门控的 `windows` 依赖已经在这个 crate 上。
//!
//! 读不到时返回 `Err`，**不返回 0**：0 在"剩余空间"这一栏会被读成"盘满了"，
//! 那是比"不知道"糟糕得多的假答案。

/// 一个卷的空间。`total` 是卷的容量，`free` 是**当前用户可写**的剩余
/// （`lpFreeBytesAvailableToCaller`，不是 `lpTotalFreeBytes`——配额受限时两者不同，
/// 而用户问的"还能写多少"是前者）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub total: u64,
    pub free: u64,
}

impl Space {
    /// 已用比例，夹在 0..=1。`total == 0` 时返回 `None` 而不是除零或 0.0——
    /// 0.0 会被画成"完全空的盘"。
    pub fn used_ratio(&self) -> Option<f32> {
        if self.total == 0 {
            return None;
        }
        let used = self.total.saturating_sub(self.free);
        Some((used as f64 / self.total as f64) as f32)
    }
}

/// 问 `path` 所在卷的空间。`path` 给文件或目录都行，可以还不存在。
#[cfg(windows)]
pub fn of(path: &std::path::Path) -> Result<Space, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    // `GetDiskFreeSpaceExW` 只吃目录：给它文件路径会报
    // `ERROR_DIRECTORY (0x8007010B)「目录名称无效」`——这条是实测踩出来的，
    // 而"问某个 exe 在哪卷上还剩多少"恰恰是最自然的用法。
    let as_dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| path.to_path_buf())
    };
    let target = existing_ancestor(&as_dir);
    let wide = {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(&target)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let mut free = 0u64;
    let mut total = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free),
            Some(&mut total),
            None,
        )
        .map_err(|e| format!("GetDiskFreeSpaceExW({}) 失败：{e}", target.display()))?;
    }
    Ok(Space { total, free })
}

#[cfg(not(windows))]
pub fn of(path: &std::path::Path) -> Result<Space, String> {
    Err(format!(
        "卷空间查询目前只实现了 Windows（{}）",
        path.display()
    ))
}

/// 走到第一个存在的祖先。`GetDiskFreeSpaceExW` 要的是"卷上的一个路径"，
/// 路径本身不存在时它会失败——而"看一个还没建的目录会在哪卷上有多少空间"
/// 是个合理问题。
fn existing_ancestor(path: &std::path::Path) -> std::path::PathBuf {
    let mut cur = path;
    loop {
        if cur.exists() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) if p != cur => cur = p,
            _ => return cur.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_ratio_is_clamped_and_none_on_zero_total() {
        assert_eq!(Space { total: 0, free: 0 }.used_ratio(), None);
        assert_eq!(
            Space {
                total: 100,
                free: 25
            }
            .used_ratio(),
            Some(0.75)
        );
        // free > total（配额/压缩盘上真会出现）不能算出负比例
        assert_eq!(
            Space {
                total: 100,
                free: 150
            }
            .used_ratio(),
            Some(0.0)
        );
    }

    #[cfg(windows)]
    #[test]
    fn asks_the_current_volume() {
        let here = std::env::current_exe().unwrap();
        let s = of(&here).expect("当前 exe 所在卷该问得出来");
        assert!(s.total > 0, "容量不可能是 0，拿到 {}", s.total);
        assert!(s.free <= s.total, "free {} > total {}", s.free, s.total);
    }

    /// 不存在的路径要能问（退到祖先），而不是报错。
    #[cfg(windows)]
    #[test]
    fn missing_path_falls_back_to_an_existing_ancestor() {
        let ghost = std::env::temp_dir()
            .join("fmnr-no-such-dir-9f3a2b")
            .join("deeper");
        assert!(!ghost.exists());
        let s = of(&ghost).expect("不存在的目录应退到祖先再问");
        assert!(s.total > 0);
    }

    #[test]
    fn existing_ancestor_terminates_on_a_root() {
        // 盘根没有可去的父目录；这里不能无限循环
        let p = std::path::Path::new("/definitely-not-here-xyz");
        let a = existing_ancestor(p);
        assert!(!a.to_string_lossy().is_empty());
    }
}
