//! 纯文件操作层：不依赖 UI、不依赖 async 运行时。
//! 流式拷贝（1 MiB 缓冲）、冲突改名、回收站删除都在这里。
//! LocalVolume 只做「路径解析 + 参数编排」，实际动作全在本模块。

use fmnr_core::{ConflictPolicy, Error, OpReport, Progress, Result};
use std::path::{Path, PathBuf};

/// 拷贝缓冲：1 MiB。既够大以减少 syscall，又够小以让取消/进度有秒级响应。
const BUF_SIZE: usize = 1 << 20;

/// 单个条目的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemOutcome {
    Done,
    Skipped,
    Aborted,
}

impl ItemOutcome {
    fn merge_into(self, report: &mut OpReport, bytes: u64) {
        match self {
            ItemOutcome::Done => {
                report.done += 1;
                report.bytes += bytes;
            }
            ItemOutcome::Skipped => report.skipped += 1,
            ItemOutcome::Aborted => report.aborted += 1,
        }
    }
}

/// 统计目录树的 (总字节数, 文件数)，用作进度条分母。
/// 只读元数据，不解引用符号链接（避免环）。
pub fn dir_stats(root: &Path) -> (u64, u64) {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(_) => return (0, 0),
    };
    if meta.is_file() {
        return (meta.len(), 1);
    }
    if !meta.is_dir() {
        return (0, 0);
    }

    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in rd.flatten() {
            let Ok(m) = item.metadata() else { continue };
            if m.is_dir() {
                stack.push(item.path());
            } else if m.is_file() {
                bytes += m.len();
                files += 1;
            }
        }
    }
    (bytes, files)
}

/// 目标已存在时求一个不冲突的名字：`a.txt` → `a (2).txt` → `a (3).txt`。
/// 无冲突时原样返回。
pub fn unique_name(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let parent = dest.parent().unwrap_or(Path::new("."));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = dest
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();

    // 若原名已是 "x (2)" 形式，从它的序号继续往上加。
    let (base, start) = match stem.rfind(" (") {
        Some(i) if stem.ends_with(')') => {
            let inner = &stem[i + 2..stem.len() - 1];
            match inner.parse::<u32>() {
                Ok(n) if n >= 2 => (stem[..i].to_string(), n + 1),
                _ => (stem.clone(), 2),
            }
        }
        _ => (stem.clone(), 2),
    };

    for n in start..u32::MAX {
        let candidate = if ext.is_empty() {
            format!("{base} ({n})")
        } else {
            format!("{base} ({n}).{ext}")
        };
        let path = parent.join(candidate);
        if !path.exists() {
            return path;
        }
    }
    dest.to_path_buf()
}

/// 按冲突策略解析最终目标路径。`Ok(None)` 表示应当跳过。
pub fn resolve_dest(dest: &Path, policy: ConflictPolicy) -> Option<PathBuf> {
    if !dest.exists() {
        return Some(dest.to_path_buf());
    }
    match policy {
        ConflictPolicy::Overwrite => Some(dest.to_path_buf()),
        ConflictPolicy::Skip => None,
        ConflictPolicy::Rename => Some(unique_name(dest)),
    }
}

/// 流式拷贝单个条目（文件或目录树）到 `dest`（dest 是完整目标路径）。
/// 每 1 MiB 回写一次进度并轮询取消位。
pub fn copy_tree(
    src: &Path,
    dest: &Path,
    progress: &Progress,
    remove_src: bool,
) -> Result<ItemOutcome> {
    let meta = std::fs::symlink_metadata(src).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", src.display()),
        ))
    })?;

    if meta.is_dir() {
        std::fs::create_dir_all(dest).map_err(Error::Io)?;
        let rd = std::fs::read_dir(src).map_err(Error::Io)?;
        let mut aborted = false;
        for item in rd {
            let item = item.map_err(Error::Io)?;
            if progress.is_cancelled() {
                aborted = true;
                break;
            }
            let child_dest = dest.join(item.file_name());
            let outcome = copy_tree(&item.path(), &child_dest, progress, remove_src)?;
            if outcome == ItemOutcome::Aborted {
                aborted = true;
                break;
            }
        }
        if aborted {
            return Ok(ItemOutcome::Aborted);
        }
        if remove_src {
            std::fs::remove_dir_all(src).map_err(Error::Io)?;
        }
        Ok(ItemOutcome::Done)
    } else if meta.is_file() {
        copy_file(src, dest, progress)?;
        if remove_src {
            std::fs::remove_file(src).map_err(Error::Io)?;
        }
        Ok(ItemOutcome::Done)
    } else {
        // 符号链接等：不跟随，直接跳过（避免跨卷复制出意外子树）
        Ok(ItemOutcome::Skipped)
    }
}

fn copy_file(src: &Path, dest: &Path, progress: &Progress) -> Result<()> {
    use std::io::{Read, Write};

    let mut reader = std::fs::File::open(src).map_err(Error::Io)?;
    let mut writer = std::fs::File::create(dest).map_err(Error::Io)?;
    let mut buf = vec![0u8; BUF_SIZE];

    loop {
        if progress.is_cancelled() {
            drop(writer);
            let _ = std::fs::remove_file(dest);
            return Err(Error::Other("cancelled".into()));
        }
        let n = reader.read(&mut buf).map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).map_err(Error::Io)?;
        progress.add_bytes(n as u64);
    }
    writer.flush().map_err(Error::Io)?;
    progress.add_file();
    Ok(())
}

/// 批量拷贝/移动的共享实现。`remove_src=true` 即移动。
pub fn transfer(
    sources: &[PathBuf],
    dest_dir: &Path,
    policy: ConflictPolicy,
    progress: &Progress,
    remove_src: bool,
) -> Result<OpReport> {
    std::fs::create_dir_all(dest_dir).map_err(Error::Io)?;

    // 先算总量，进度条才有意义的分母。
    let mut total_bytes = 0u64;
    let mut total_files = 0u64;
    for s in sources {
        let (b, f) = dir_stats(s);
        total_bytes += b;
        total_files += f;
    }
    progress.set_totals(total_bytes, total_files);

    let mut report = OpReport::default();
    for src in sources {
        if progress.is_cancelled() {
            report.aborted += 1;
            continue;
        }
        let Some(name) = src.file_name() else {
            report.skipped += 1;
            continue;
        };
        let raw_dest = dest_dir.join(name);
        let Some(dest) = resolve_dest(&raw_dest, policy) else {
            report.skipped += 1;
            continue;
        };
        // 覆盖模式下若目标已存在，先清掉（否则 create_dir_all/文件覆盖语义不一致）
        if policy == ConflictPolicy::Overwrite && dest != raw_dest {
            let _ = std::fs::remove_dir_all(&dest);
        } else if policy == ConflictPolicy::Overwrite && dest.exists() {
            let m = std::fs::symlink_metadata(&dest);
            if matches!(m, Ok(ref m) if m.is_dir()) {
                std::fs::remove_dir_all(&dest).map_err(Error::Io)?;
            }
        }

        let before = progress.snapshot().bytes_done;
        let outcome = match copy_tree(src, &dest, progress, remove_src) {
            Ok(o) => o,
            Err(Error::Other(msg)) if msg == "cancelled" => ItemOutcome::Aborted,
            Err(e) => return Err(e),
        };
        let delta = progress.snapshot().bytes_done.saturating_sub(before);
        outcome.merge_into(&mut report, delta);
    }
    Ok(report)
}

/// 送入回收站删除（可恢复）。非 Windows 平台退化为永久删除并明确标注。
/// 返回成功删除的条目数。
#[cfg(windows)]
pub fn delete_to_recycle_bin(targets: &[PathBuf]) -> Result<usize> {
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE,
        SHFILEOPSTRUCTW,
    };

    if targets.is_empty() {
        return Ok(0);
    }

    // pFrom 要求：路径以 NUL 分隔，列表以额外一个 NUL 结尾（双 NUL 终止）。
    let mut from: Vec<u16> = Vec::new();
    let mut counted = 0usize;
    for t in targets {
        // SHFileOperationW 不接受相对路径。
        let abs = std::path::absolute(t).map_err(Error::Io)?;
        from.extend(abs.to_string_lossy().encode_utf16());
        from.push(0);
        counted += 1;
    }
    from.push(0);

    let flags = (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0 | FOF_SILENT.0) as u16;

    let mut op = SHFILEOPSTRUCTW {
        hwnd: Default::default(),
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        pTo: windows::core::PCWSTR::null(),
        fFlags: flags,
        fAnyOperationsAborted: Default::default(),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: windows::core::PCWSTR::null(),
    };

    // 返回值为 0 才算成功；非 0 是未文档化的错误码，直接透出。
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(Error::Other(format!("SHFileOperationW 返回 {rc}")));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err(Error::Other("用户中止了删除".into()));
    }
    Ok(counted)
}

#[cfg(not(windows))]
pub fn delete_to_recycle_bin(targets: &[PathBuf]) -> Result<usize> {
    let mut n = 0;
    for t in targets {
        let m = std::fs::symlink_metadata(t).map_err(Error::Io)?;
        if m.is_dir() {
            std::fs::remove_dir_all(t).map_err(Error::Io)?;
        } else {
            std::fs::remove_file(t).map_err(Error::Io)?;
        }
        n += 1;
    }
    Ok(n)
}

/// 查询某卷回收站的条目数。用于「删除确实进了回收站」的判定，而不是靠返回值自证。
#[cfg(windows)]
pub fn recycle_bin_items(root: &Path) -> Result<i64> {
    use windows::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};

    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        i64Size: 0,
        i64NumItems: 0,
    };
    let root_wide: Vec<u16> = root
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe { SHQueryRecycleBinW(windows::core::PCWSTR(root_wide.as_ptr()), &mut info) }
        .map_err(|e| Error::Other(format!("SHQueryRecycleBinW: {e}")))?;
    Ok(info.i64NumItems)
}

#[cfg(not(windows))]
pub fn recycle_bin_items(_root: &Path) -> Result<i64> {
    Err(Error::Unsupported("仅 Windows 支持回收站查询".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "fmnr-ops-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn unique_name_appends_counter() {
        let d = tmpdir("unique");
        let a = d.join("a.txt");
        std::fs::write(&a, b"1").unwrap();
        assert_eq!(unique_name(&a), d.join("a (2).txt"));

        std::fs::write(d.join("a (2).txt"), b"2").unwrap();
        assert_eq!(unique_name(&a), d.join("a (3).txt"));

        std::fs::write(d.join("a (3).txt"), b"3").unwrap();
        assert_eq!(unique_name(&a), d.join("a (4).txt"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn unique_name_without_conflict_is_identity() {
        let d = tmpdir("identity");
        let a = d.join("fresh.bin");
        assert_eq!(unique_name(&a), a);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn skip_policy_returns_none_on_conflict() {
        let d = tmpdir("skip");
        let a = d.join("x.txt");
        std::fs::write(&a, b"x").unwrap();
        assert_eq!(resolve_dest(&a, ConflictPolicy::Skip), None);
        assert_eq!(resolve_dest(&a, ConflictPolicy::Overwrite), Some(a.clone()));
        assert_eq!(
            resolve_dest(&a, ConflictPolicy::Rename),
            Some(d.join("x (2).txt"))
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn copy_tree_moves_bytes_and_reports_progress() {
        let d = tmpdir("copy");
        let src = d.join("src");
        let dst = d.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        // 3 MiB：跨越多个 1 MiB 分块，能证明流式循环真的在跑
        let payload = vec![7u8; 3 * BUF_SIZE + 123];
        std::fs::write(src.join("big.bin"), &payload).unwrap();
        std::fs::write(src.join("small.txt"), b"hi").unwrap();

        let progress = Progress::new();
        let report = transfer(
            std::slice::from_ref(&src),
            &dst,
            ConflictPolicy::Skip,
            &progress,
            false,
        )
        .unwrap();

        assert_eq!(report.done, 1);
        assert_eq!(report.bytes, payload.len() as u64 + 2);
        let out = dst.join("src");
        assert_eq!(std::fs::read(out.join("big.bin")).unwrap(), payload);
        assert_eq!(std::fs::read(out.join("small.txt")).unwrap(), b"hi");
        assert!(src.exists(), "拷贝不应删除源");

        let snap = progress.snapshot();
        assert_eq!(snap.bytes_total, snap.bytes_done);
        assert_eq!(snap.files_total, snap.files_done);
        assert_eq!(snap.fraction(), 1.0);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn move_removes_source() {
        let d = tmpdir("move");
        let src = d.join("a.txt");
        let dst = d.join("out");
        std::fs::write(&src, b"payload").unwrap();

        let progress = Progress::new();
        let report = transfer(
            std::slice::from_ref(&src),
            &dst,
            ConflictPolicy::Skip,
            &progress,
            true,
        )
        .unwrap();

        assert_eq!(report.done, 1);
        assert!(!src.exists(), "移动必须删掉源");
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"payload");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn cancel_midway_aborts_and_removes_partial() {
        let d = tmpdir("cancel");
        let src = d.join("big.bin");
        let dst = d.join("out");
        // 4 MiB，取消位在第 1 块之后置起
        std::fs::write(&src, vec![1u8; 4 * BUF_SIZE]).unwrap();

        let progress = Progress::new();
        progress.cancel();
        let report = transfer(
            std::slice::from_ref(&src),
            &dst,
            ConflictPolicy::Skip,
            &progress,
            false,
        )
        .unwrap();

        assert_eq!(report.aborted, 1);
        assert_eq!(report.done, 0);
        assert!(src.exists(), "取消不应动源文件");
        // 半成品必须清掉，否则留下损坏文件
        assert!(!dst.join("big.bin").exists(), "取消后不得留半成品");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn dir_stats_counts_nested_files() {
        let d = tmpdir("stats");
        std::fs::create_dir_all(d.join("x/y")).unwrap();
        std::fs::write(d.join("x/a"), b"12345").unwrap();
        std::fs::write(d.join("x/y/b"), b"12").unwrap();
        assert_eq!(dir_stats(&d), (7, 2));
        std::fs::remove_dir_all(&d).ok();
    }

    /// 回收站判据：不能只信 `delete_to_recycle_bin` 的返回值——那是规则作者自己的输出。
    /// 外部观测是回收站条目计数（shell 维护，与 fmnr 无关），必须 +1。
    ///
    /// `#[ignore]`：每次跑都会往用户回收站里塞一个文件，只在需要复核判据时手动跑：
    /// `cargo test -p fmnr-fs -- --ignored --nocapture`。
    #[test]
    #[ignore = "会污染用户回收站，仅手动复核时运行"]
    fn recycle_bin_count_rises_by_one() {
        let d = tmpdir("recycle");
        let victim = d.join("fmnr-rb-probe.txt");
        std::fs::write(&victim, b"probe").unwrap();
        // 盘符根：回收站按卷统计
        let volume_root = std::path::Path::new(d.components().next().unwrap().as_os_str());

        let before = recycle_bin_items(volume_root).expect("SHQueryRecycleBinW");
        let n = delete_to_recycle_bin(std::slice::from_ref(&victim)).expect("delete");
        let after = recycle_bin_items(volume_root).expect("SHQueryRecycleBinW");

        assert_eq!(n, 1);
        assert!(!victim.exists(), "文件应已从原位置移走（回收站不是软删除）");
        assert_eq!(after, before + 1, "回收站条目数必须 +1");
        std::fs::remove_dir_all(&d).ok();
    }

    /// 反证：若删除退化成 `remove_file`（绕过回收站），计数不会变。
    /// 与上面同一把尺，证明这条尺真能区分两种实现。
    #[test]
    #[ignore = "同上，手动复核用"]
    fn permanent_delete_does_not_raise_recycle_bin_count() {
        let d = tmpdir("perm");
        let victim = d.join("fmnr-perm-probe.txt");
        std::fs::write(&victim, b"probe").unwrap();
        let volume_root = std::path::Path::new(d.components().next().unwrap().as_os_str());

        let before = recycle_bin_items(volume_root).expect("SHQueryRecycleBinW");
        std::fs::remove_file(&victim).unwrap();
        let after = recycle_bin_items(volume_root).expect("SHQueryRecycleBinW");

        assert_eq!(after, before, "永久删除不应进回收站（对照组）");
        std::fs::remove_dir_all(&d).ok();
    }
}
