//! 目标重名时的**命名规则**——本仓只有这一份。
//!
//! 为什么单独一格：同一条规则有两个消费者，且它们的"存在性"问法不同——
//! `fmnr-fs::ops` 直接问 `Path::exists`，跨卷搬运（`crate::transfer`）必须**问目标卷**
//! （拿本地文件系统的 `exists` 去问远程卷会把"本地没有"当成"目标没有"）。
//! 两边各写一遍的话，能共用的只有"探测"那三行循环，而**格式与续号**这份规则一旦分叉，
//! 同一个文件从两条入口拖进同一个目录会得到两个不同的名字。
//! 本仓已经为此付过一次学费（`.lnk` 的 builder、归档那句"懒加载目录"、共享掩码政策）。

use std::path::{Path, PathBuf};

/// `dest` 的第 `n` 个候选名：`a.txt` → `a (n).txt`；无扩展名时 `README` → `README (n)`。
/// 父目录与 `dest` 相同。
pub fn candidate_name(dest: &Path, n: u32) -> PathBuf {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let body = match stem.rfind(" (") {
        // 原名已经带 ` (2)` 这类序号时，只换序号、不重复追加。
        Some(i) if stem.ends_with(')') && inner_index(&stem[i + 2..stem.len() - 1]).is_some() => {
            stem[..i].to_string()
        }
        _ => stem.clone(),
    };
    let candidate = match dest.extension().map(|e| e.to_string_lossy().to_string()) {
        Some(ext) if !ext.is_empty() => format!("{body} ({n}).{ext}"),
        _ => format!("{body} ({n})"),
    };
    parent.join(candidate)
}

/// 该从几开始试：`note` → 2，`note (2)` → 3，`a (2) (3)` → 4（只看最右那一段）。
pub fn first_rename_index(stem: &str) -> u32 {
    match stem.rfind(" (") {
        Some(i) if stem.ends_with(')') => inner_index(&stem[i + 2..stem.len() - 1])
            .map(|n| n + 1)
            .unwrap_or(2),
        _ => 2,
    }
}

/// 认得出 `(2)`、`(17)` 这种序号；`()`、`(x)`、`(1)`（从 2 起才算）一律不认。
fn inner_index(inner: &str) -> Option<u32> {
    let n: u32 = inner.parse().ok()?;
    (n >= 2).then_some(n)
}
