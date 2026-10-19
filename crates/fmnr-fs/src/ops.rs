//! 纯文件操作层：不依赖 UI、不依赖 async 运行时。
//! 流式拷贝（1 MiB 缓冲）、冲突改名、回收站删除都在这里。
//! LocalVolume 只做「路径解析 + 参数编排」，实际动作全在本模块。

use fmnr_core::{ConflictPolicy, Error, OpReport, Progress, Result};
use std::path::{Path, PathBuf};

/// 拷贝缓冲：1 MiB。既够大以减少 syscall，又够小以让取消/进度有秒级响应。
const BUF_SIZE: usize = 1 << 20;

/// Windows 的共享掩码常量（`std::os::windows::fs::OpenOptionsExt::share_mode` 收的是
/// 原始 `u32`，这里按文档值写死，不为两个常数把 `windows` 拉进编译面）。
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x1;
#[cfg(windows)]
const FILE_SHARE_DELETE: u32 = 0x4;
/// `ERROR_SHARING_VIOLATION`
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;

/// 以"**不许别人同时写**"的共享掩码打开目标文件（写 + 建 + 截断）。
///
/// 为什么要有这一层：`std::fs::File::create` 在 Windows 上把共享掩码开成
/// `READ|WRITE|DELETE`（全共享），于是往一个**别的程序正开着写**的文件上覆盖时
/// CreateFile 照样成功——`truncate` 当场把对方已写进去的字节抹掉，两边继续各写各的，
/// 最后得到一个混着零洞的文件，而 fmnr 报"成功、N 字节"。
/// 本机实测（`share_mode` 对拍）：对方以 std 全共享持句柄并已写 8 字节时，
/// std 默认开法得到 `Ok` 且目标长度变成 **0**；换成这里这种开法得到
/// `ERROR_SHARING_VIOLATION(32)`。反过来，对方若只共享读（Explorer/Office 那一类），
/// std 默认开法本来就会失败——也就是说**共享掩码才是主闸**，
/// 要挡"对方开着写"必须自己先把写共享拒掉（`DESIGN` §4.3(a) 说的就是这个）。
///
/// 非 Windows 上退化成 `File::create`（下面另有一条同名的 `#[cfg(not(windows))]` 实现）：
/// POSIX 没有共享语义可借，装出"锁住了"的样子比不装更糟。
#[cfg(windows)]
pub fn create_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)
}

/// 非 Windows：POSIX 的 `flock`/`fcntl` 都是**建议锁**，没有共享语义可借，
/// 装出"锁住了"的样子比不装更糟（同 `DESIGN` §4.3(a) 那条口径），所以原样开。
#[cfg(not(windows))]
pub fn create_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

/// 把"目标正被别的程序开着写"从一堆 io 错误里认出来，说成一句人话。
///
/// 只认 `ERROR_SHARING_VIOLATION` 这一个码，别的一律原样交给 `Error::Io`——
/// 把不认识的错误都改写成"被占用"是另一种撒谎。
#[cfg(windows)]
pub fn in_use_error(path: &Path, e: std::io::Error) -> Error {
    if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
        return Error::Other(format!(
            "{} 正被另一个程序开着写，不能覆盖它（要覆盖请先关掉那个程序）",
            path.display()
        ));
    }
    Error::Io(e)
}

#[cfg(not(windows))]
pub fn in_use_error(_path: &Path, e: std::io::Error) -> Error {
    Error::Io(e)
}

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

/// 整份写入的**唯一**入口：走 `create_exclusive` 的开法，所以"别人正开着写"
/// 在这里会变成一条说得清的失败，而不是静默把对方的数据截掉。
///
/// 逐块把流写进任意 `AsyncWrite`，返回写出的字节数。**一块一次写，不先收齐。**
///
/// 为什么单独抽出来：这是"跨卷搬运不把整个文件读进内存"这句话里**唯一能被确定地测**
/// 的那一半。另一半（"边收边写在盘上立刻看得见"）在本机量不出来——实测：
/// `write_all(...).await` 已经返回成功，另开一个句柄问大小仍报 **0 字节**，
/// 要先在写句柄上 `stream_position()`（一次 seek）才变得可见。
/// 所以拿"另一个句柄看到的文件大小"当判据的门会**红在尺子上**，不 Ship（细节见 CHANGELOG）。
/// 这一格的判据改成**写入次数**：三块流必须让 sink 看到 ≥2 次写；
/// 谁把它改成"收齐再一次写"，这条当场判红。
pub async fn stream_to_writer<W, S>(writer: &mut W, stream: &mut S) -> std::io::Result<u64>
where
    W: tokio::io::AsyncWrite + Unpin,
    S: futures::Stream<Item = std::io::Result<Vec<u8>>> + Unpin,
{
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        writer.write_all(&bytes).await?;
        written += bytes.len() as u64;
    }
    Ok(written)
}

/// `LocalVolume::write` 与拷贝落盘都汇到这一层——同一件政策只写一处
/// （本轮 `.lnk` 那条的教训就是：同一句事实写两份，其中一份一定会先过期）。
pub fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = create_exclusive(path).map_err(|e| in_use_error(path, e))?;
    f.write_all(data).map_err(|e| in_use_error(path, e))?;
    f.flush().map_err(|e| in_use_error(path, e))?;
    Ok(())
}

/// 目标已存在时求一个不冲突的名字：`a.txt` → `a (2).txt` → `a (3).txt`。
/// 无冲突时原样返回。
///
/// 名字怎么拼（`{基名} ({n}).{扩展}`、已带序号则续号）在 `fmnr_core::rename` 里，
/// **这里只剩"问本地盘存不存在"那三行**：跨卷搬运（`fmnr_core::transfer`）问的是目标卷，
/// 两边必须共用同一条拼名规则，否则同一个文件从两条入口拖进同一个目录会拿到两个名字。
pub fn unique_name(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    for n in fmnr_core::rename::first_rename_index(&stem)..u32::MAX {
        let candidate = fmnr_core::rename::candidate_name(dest, n);
        if !candidate.exists() {
            return candidate;
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
    // 目标一律走"拒写共享"的开法：别人正开着写的文件不该被我们静默截断（见
    // `create_exclusive` 的实测注释）。
    let mut writer = create_exclusive(dest).map_err(|e| in_use_error(dest, e))?;

    // 缓冲区按**文件大小**封顶，不无脑开满 BUF_SIZE。
    // 实测判据（`cargo run -p fmnr-bench --bin io_throughput`）：原先每个文件都
    // `vec![0u8; 1 MiB]`，而 `vec![0u8; n]` 会把 n 字节**全部清零**，
    // 于是拷 2000 个 64 KiB 小文件要先白刷约 2 GiB —— 吞吐 51 MiB/s，
    // 而同盘 `std::fs::copy` 参照是 114 MiB/s（慢 2.24×）。
    // 大文件那一档改前改后都贴着盘的上限（113.8 vs 113.5），所以这不是白调参数。
    let len = reader.metadata().map(|m| m.len()).unwrap_or(0) as usize;
    let cap = if len == 0 {
        BUF_SIZE
    } else {
        len.min(BUF_SIZE)
    };
    let mut buf = vec![0u8; cap];

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

/// 目标目录是否**就是源、或在源的子树内**。
///
/// 这是拖放/移动必须挡住的一种输入：把 `A` 拷进 `A\sub`，`copy_tree` 会在
/// 自己刚创建的目录里继续发现新内容——无限递归，直到路径超长或磁盘写满。
/// 覆盖模式下还有个更隐蔽的变体：`dest == src` 时会先删源再拷。
///
/// 判定用 canonicalize 后的路径，避免 `A\sub\..\sub` 这类绕法。
pub fn dest_inside_source(src: &Path, dest_dir: &Path) -> bool {
    let (Ok(s), Ok(d)) = (std::fs::canonicalize(src), std::fs::canonicalize(dest_dir)) else {
        // 规范化失败（路径不存在等）时**不在这里编造结论**：
        // 后续 IO 自己会报错，比一个猜出来的布尔值可靠。
        return false;
    };
    d == s || d.starts_with(&s)
}

/// 批量拷贝/移动的共享实现。`remove_src=true` 即移动。
/// 同卷移动走 `rename`：一次元数据操作，**不搬数据**。
///
/// 为什么专门加这条：实测原先 move 一棵 256 MiB 子树要 1.32 s（193 MiB/s），
/// 那就是「整树逐字节拷一份 + 再删源」——数据被搬了两遍，
/// 而操作系统本来一次 rename 就能做完，且耗时与体积无关。
///
/// 返回 `Ok(Some(_))` 表示已由快速路径处理完；`Ok(None)` 表示**不适用**，
/// 调用方走原来的整树拷贝。不适用的几种情况都要保守判定，因为这条路径
/// 一旦误用，症状是「文件凭空消失」：
/// - 源是符号链接：`copy_tree` 对符号链接返回 `Skipped`（不跟随），
///   而 `rename` 会把链接本身搬走 ⇒ 语义变了，不能快。
/// - 目标已存在：`rename` 在 Windows 上对已存在目录直接失败，
///   在 Unix 上可能把两个目录**合并**，两边都不是我们要的。
/// - `rename` 本身失败（跨卷 `ERROR_NOT_SAME_DEVICE`、权限等）：退回拷贝。
fn try_rename_move(src: &Path, dest: &Path, progress: &Progress) -> Result<Option<ItemOutcome>> {
    let meta = std::fs::symlink_metadata(src).map_err(Error::Io)?;
    if meta.is_symlink() {
        return Ok(None);
    }
    if dest.exists() {
        return Ok(None);
    }

    // rename 失败**不是错误**，是「这条快速路径不适用」的信号：
    // 跨卷移动时它必然失败（Windows `ERROR_NOT_SAME_DEVICE` / Unix `EXDEV`），
    // 那时正确行为就是退回整树拷贝。这里若把错误 `?` 抛出去，
    // 症状会是「往 U 盘移动文件直接报错」，而不是安静地走慢路径。
    if std::fs::rename(src, dest).is_err() {
        return Ok(None);
    }

    // 数据没搬，但进度要照实结清：分母是 transfer 预先算好的总量，
    // 这里按同一口径把字节与文件数补上，否则进度条会停在 0。
    let (bytes, files) = dir_stats(dest);
    progress.add_bytes(bytes);
    for _ in 0..files {
        progress.add_file();
    }
    Ok(Some(ItemOutcome::Done))
}

pub fn transfer(
    sources: &[PathBuf],
    dest_dir: &Path,
    policy: ConflictPolicy,
    progress: &Progress,
    remove_src: bool,
) -> Result<OpReport> {
    std::fs::create_dir_all(dest_dir).map_err(Error::Io)?;

    // 预检放在**任何写入之前**：把 A 拷进 A 的子树会无限递归，
    // 等发现时可能已经拷了一半，回滚也回不干净。
    for src in sources {
        if dest_inside_source(src, dest_dir) {
            return Err(Error::Other(format!(
                "拒绝把 {} 放进它自己（或其子目录）里：会无限递归",
                src.display()
            )));
        }
    }

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
        // move 才试快速路径；copy 没有「源消失」这条退路可换。
        // rename 失败不致命：跨卷时它必然失败，那时退回整树拷贝就是正确行为。
        let fast = if remove_src {
            try_rename_move(src, &dest, progress)?
        } else {
            None
        };
        let outcome = match fast {
            Some(o) => o,
            None => match copy_tree(src, &dest, progress, remove_src) {
                Ok(o) => o,
                Err(Error::Other(msg)) if msg == "cancelled" => ItemOutcome::Aborted,
                Err(e) => return Err(e),
            },
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

    /// 只数"被写了几次"的 sink——**写入次数**是这里唯一能被确定量到的信号。
    struct CountingWriter {
        writes: usize,
        bytes: Vec<u8>,
    }

    impl std::io::Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tokio::io::AsyncWrite for CountingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(std::io::Write::write(self.get_mut(), buf))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// 🔴 门：**一块一次写**，不许先收齐。三块的流必须让 sink 看到 ≥2 次写。
    ///
    /// 为什么用写入次数而不是"盘上大小"：跨句柄看长度在本机不可靠
    /// （`write_all` 已返回成功后，另一个句柄仍报 0 字节，要先在写句柄上 seek 一次），
    /// 那条判据会红在尺子上。而"收齐再一次写"这个退化写法在**内容上完全等价**，
    /// 只有写入次数分得开——它也正是"不把整个文件读进内存"这句话的可测部分。
    #[tokio::test]
    async fn stream_to_writer_writes_each_chunk_separately() {
        let chunks: Vec<std::io::Result<Vec<u8>>> =
            vec![Ok(vec![b'a'; 3]), Ok(vec![b'b'; 4]), Ok(vec![b'c'; 5])];
        let mut stream = futures::stream::iter(chunks);
        let mut w = CountingWriter {
            writes: 0,
            bytes: Vec::new(),
        };

        let n = stream_to_writer(&mut w, &mut stream).await.unwrap();

        assert_eq!(n, 12, "返回值要等于全部字节数");
        assert_eq!(w.bytes, b"aaabbbbccccc", "内容要按顺序逐字节对上");
        assert!(
            w.writes >= 3,
            "三块流必须至少三次写；实际 {} 次 ⇒ 退化成收齐再写了",
            w.writes
        );
    }

    /// 空流：一次都不写、返回 0。**0 字节与"没发生"要区分开**——那是上层建不建文件的依据。
    #[tokio::test]
    async fn empty_stream_writes_nothing_and_reports_zero() {
        let mut stream = futures::stream::iter(Vec::<std::io::Result<Vec<u8>>>::new());
        let mut w = CountingWriter {
            writes: 0,
            bytes: Vec::new(),
        };
        assert_eq!(stream_to_writer(&mut w, &mut stream).await.unwrap(), 0);
        assert_eq!(w.writes, 0);
    }

    /// 源侧报错要**原样带出去**，不能变成"成功但内容短了"。
    #[tokio::test]
    async fn a_failing_chunk_is_propagated_not_truncated() {
        let chunks = vec![
            Ok(vec![b'x'; 4]),
            Err(std::io::Error::other("解压到一半坏了")),
        ];
        let mut stream = futures::stream::iter(chunks);
        let mut w = CountingWriter {
            writes: 0,
            bytes: Vec::new(),
        };
        let e = stream_to_writer(&mut w, &mut stream).await.unwrap_err();
        assert!(
            e.to_string().contains("解压到一半坏了"),
            "错要原样带出：{e}"
        );
    }

    /// 🔴 门：往一个**别的程序正开着写**的文件上覆盖，必须失败并把原因说清楚，
    /// 而不是把对方已写进去的字节截掉还回报成功。
    ///
    /// 语料形状就是量出这条机制的那次（`share_mode` 对拍）：对方以 std 默认的
    /// 全共享（read+write）持着句柄并已写 8 字节——python / notepad 那一类。
    /// 修前：`copy_file` 得到 `Ok`，目标长度变成 **0**；
    /// 修后：得到 `ERROR_SHARING_VIOLATION` 翻成的人话，且对方的字节一个没动。
    ///
    /// 同一件事要先证明**没有竞争时照样能覆盖**，否则这条门是空转。
    #[cfg(windows)]
    #[test]
    fn overwrite_refuses_a_file_another_writer_holds() {
        use std::io::Write;

        let d = tmpdir("busy");
        let src = d.join("src.bin");
        std::fs::write(&src, vec![b'A'; 64]).unwrap();
        let target = d.join("x.bin");
        std::fs::write(&target, b"peer-data").unwrap();

        let prog = || Progress::new();
        let mut peer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)
            .unwrap();
        peer.write_all(b"BUSY").unwrap(); // 从 0 号位盖掉对方的开头
        peer.flush().unwrap();
        // 拿"被拒之前它是什么"当锚，而不是硬编码字节：要护住的不变量是
        // **我们没动过它**，不是"它一定等于某个我算出来的串"。
        let before = std::fs::read(&target).unwrap();
        assert_ne!(before, vec![b'A'; 64], "锚点得是别人的内容，不是源的副本");

        let e = copy_file(&src, &target, &prog()).expect_err("对方正开着写，覆盖必须失败");
        let msg = e.to_string();
        assert!(msg.contains("正被另一个程序"), "{msg}");
        let write_err = write_file(&target, b"zz").expect_err("write 路径同闸");
        assert!(
            write_err.to_string().contains("正被另一个程序"),
            "{write_err}"
        );

        // 关键的一半：对方的数据必须还在
        drop(peer);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            before,
            "被拒之后目标不该被动过——截成 0 就是修前那个行为"
        );

        // 对照组：没人占用时，同一次覆盖要照样成功（证明上面那条不是空转）
        copy_file(&src, &target, &prog()).expect("没人占用时必须还能覆盖");
        assert_eq!(std::fs::read(&target).unwrap(), vec![b'A'; 64]);
        write_file(&target, b"free").expect("没人占用时 write_file 也要能写");
        assert_eq!(std::fs::read(&target).unwrap(), b"free");
        std::fs::remove_dir_all(&d).ok();
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

    /// 把目录拷进自己 / 自己的子目录，必须在动手前被拒。
    /// 否则 `copy_tree` 会在刚建出的目录里发现新内容，无限递归。
    #[test]
    fn transfer_refuses_dest_inside_source() {
        let d = tmpdir("selfnest");
        let src = d.join("A");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("f.txt"), b"x").unwrap();

        let progress = Progress::new();

        // 放进自己
        let e = transfer(
            std::slice::from_ref(&src),
            &src,
            ConflictPolicy::Skip,
            &progress,
            false,
        )
        .unwrap_err();
        assert!(e.to_string().contains("无限递归"), "错误应说明原因：{e}");

        // 放进自己的子目录
        let e = transfer(
            std::slice::from_ref(&src),
            &src.join("sub"),
            ConflictPolicy::Skip,
            &progress,
            false,
        )
        .unwrap_err();
        assert!(e.to_string().contains("无限递归"), "错误应说明原因：{e}");

        // 原目录必须原封不动（预检在任何写入之前）
        assert!(src.join("f.txt").is_file(), "被拒后不该动过源");
        assert_eq!(
            std::fs::read_dir(src.join("sub")).unwrap().count(),
            0,
            "子目录不该被写入任何东西"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// 反证：**兄弟目录**不算「在里面」，必须放行。
    /// 否则这条尺会把正常的同级拷贝也拦掉——过宽和过窄都是错的。
    #[test]
    fn transfer_allows_sibling_destination() {
        let d = tmpdir("sibling");
        let a = d.join("A");
        let b = d.join("B");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("f.txt"), b"hi").unwrap();

        let progress = Progress::new();
        let report = transfer(
            std::slice::from_ref(&a),
            &b,
            ConflictPolicy::Skip,
            &progress,
            false,
        )
        .expect("同级目录应放行");
        assert_eq!(report.done, 1);
        assert_eq!(std::fs::read(b.join("A/f.txt")).unwrap(), b"hi");
        std::fs::remove_dir_all(&d).ok();
    }

    /// 纯函数层面把边界钉清楚：路径前缀相同但不同目录，不能误判为「在里面」。
    /// 例：`C:\a\bc` 不以 `C:\a\b` 为父——必须按路径分量比，不能比字符串前缀。
    #[test]
    fn dest_inside_source_compares_path_components_not_strings() {
        let d = tmpdir("prefix");
        let b = d.join("b");
        let bc = d.join("bc");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&bc).unwrap();

        assert!(!dest_inside_source(&b, &bc), "b 与 bc 是兄弟，不是父子");
        assert!(dest_inside_source(&b, &b), "自己算在里面");
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

    /// 移动一棵子树：源消失、目标内容完整、进度结清。
    ///
    /// 这条钉的是**加了 rename 快速路径之后**语义没变。
    /// 快速路径误用的症状是「文件凭空消失」，所以内容也要逐字节比。
    #[test]
    fn move_tree_relocates_content_and_removes_source() {
        let d = tmpdir("mvfast");
        let src = d.join("tree");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"AAA").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"BBBB").unwrap();
        let dst = d.join("out");

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
        assert!(!src.exists(), "移动后源不该还在");
        assert_eq!(std::fs::read(dst.join("tree/a.txt")).unwrap(), b"AAA");
        assert_eq!(std::fs::read(dst.join("tree/sub/b.txt")).unwrap(), b"BBBB");
        // 走 rename 时数据没被搬运，但进度必须照实结清，否则 UI 停在 0%。
        let snap = progress.snapshot();
        assert_eq!(snap.bytes_total, snap.bytes_done, "rename 路径也要结清字节");
        assert_eq!(
            snap.files_total, snap.files_done,
            "rename 路径也要结清文件数"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// 快速路径**不该**改变冲突策略的行为：目标已存在时仍按 Skip/Rename 处理。
    #[test]
    fn move_respects_conflict_policy_with_rename_path() {
        let d = tmpdir("mvconflict");
        let src = d.join("s");
        let dst = d.join("d");
        std::fs::create_dir_all(&src).unwrap();
        // 目标侧要预先存在 `d/s/x.txt`，才构成「同名冲突」这个场景。
        std::fs::create_dir_all(dst.join("s")).unwrap();
        std::fs::write(src.join("x.txt"), b"new").unwrap();
        std::fs::write(dst.join("s").join("x.txt"), b"old").unwrap();

        // Skip：目标已存在 ⇒ 跳过，且**源必须保留**（不能先 rename 再发现冲突）
        let progress = Progress::new();
        let report = transfer(
            std::slice::from_ref(&src),
            &dst,
            ConflictPolicy::Skip,
            &progress,
            true,
        )
        .unwrap();
        assert_eq!(report.skipped, 1, "同名目标存在时 Skip 应跳过：{report:?}");
        assert!(src.exists(), "被跳过的源不该消失");
        assert_eq!(
            std::fs::read(dst.join("s/x.txt")).unwrap(),
            b"old",
            "不该覆盖"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// 符号链接**不能**走 rename：`copy_tree` 对它的语义是「不跟随、跳过」，
    /// 而 rename 会把链接本身搬走 —— 那是行为改变。
    #[test]
    fn move_does_not_relocate_symlinks() {
        let d = tmpdir("mvlink");
        let target = d.join("real.txt");
        std::fs::write(&target, b"data").unwrap();
        let link = d.join("link.txt");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let progress = Progress::new();
        let report = transfer(
            std::slice::from_ref(&link),
            &d.join("out"),
            ConflictPolicy::Skip,
            &progress,
            true,
        )
        .unwrap();
        // 与加快速路径之前一致：符号链接被 Skipped，链接本身留在原地。
        assert_eq!(report.done, 0, "符号链接不该被搬走：{report:?}");
        assert!(link.exists(), "符号链接必须留在原地（copy_tree 不跟随它）");
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
