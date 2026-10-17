//! 「找重复」的后台扫描。
//!
//! 形状照 `usage.rs`：一条后台线程 + 通道回传累计快照 + 一个取消标志。
//! 两件事必须说清楚，否则界面会给出假结论：
//! 1. **半截清单看起来和完整清单一模一样** ⇒ `aborted` 要一路带到界面；
//! 2. **读不到的文件不是"没有重复"** ⇒ `skipped` 要显示条数和原因。
//!
//! 缓存（`remember`）默认关：开着会把 `(卷号, 文件索引, mtime, size)` 四元组
//! 和哈希写到盘上的一个文本文件里。用户原话是"可以选择不记住"，
//! 所以默认就是不记住，开关在设置里。

use fmnr_fs::dupes::{self, DupGroup, Skipped};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Instant;

/// 一次扫描到目前为止的结果。
#[derive(Debug, Clone)]
pub struct Found {
    pub root: PathBuf,
    pub groups: Vec<DupGroup>,
    /// 已经过完哈希阶段的文件数
    pub scanned: u64,
    /// 收集到的候选文件数（大小分组后仍可能重复的那些）
    pub candidates: usize,
    pub skipped: Vec<Skipped>,
    pub aborted: bool,
    pub done: bool,
    /// 扫描没能跑起来（root 读不了、线程起不来）。与"跑了但没有重复"不同。
    pub error: Option<String>,
}

impl Found {
    fn new(root: PathBuf) -> Self {
        Found {
            root,
            groups: Vec::new(),
            scanned: 0,
            candidates: 0,
            skipped: Vec::new(),
            aborted: false,
            done: false,
            error: None,
        }
    }

    /// 每组留一个、其余可回收的字节数。界面顶部那个"能省多少"就是它。
    ///
    /// 口径要写死：每组**保留第一个**，所以可回收 = `(n-1) * size`。
    /// 报成 `n * size` 会把用户要留的那份也算进收益里。
    pub fn reclaimable(&self) -> u64 {
        self.groups
            .iter()
            .map(|g| {
                g.size
                    .saturating_mul(g.paths.len().saturating_sub(1) as u64)
            })
            .sum()
    }
}

pub struct Running {
    cancel: Arc<AtomicBool>,
    rx: Receiver<Found>,
    latest: Option<Found>,
    root: PathBuf,
}

impl Running {
    /// `cache_file` 给 Some 表示"记住"：开扫前读回、扫完后写回。
    /// 给 None 就是 ephemeral——什么都不往盘上写。
    pub fn start(root: PathBuf, cache_file: Option<PathBuf>) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        // 界面手上的那个 Arc 必须和线程里的是同一个：
        // 早先写成 `Arc::new(AtomicBool::new(false))` 各造一个，
        // 「取消」按钮就只是把一面没人看的旗子举起来。
        let publisher = Arc::clone(&cancel);
        let (tx, rx) = channel();
        let thread_root = root.clone();
        std::thread::Builder::new()
            .name("fmnr-dupes".into())
            .spawn(move || run_scan(thread_root, cache_file, cancel, tx))
            .expect("spawn dupes thread");

        Running {
            cancel: publisher,
            rx,
            latest: None,
            root,
        }
    }

    /// 拉一份最新快照。返回是否有新的。
    pub fn poll(&mut self) -> bool {
        let mut got = false;
        while let Ok(next) = self.rx.try_recv() {
            // 只收同一棵树的快照；旧扫描在取消后可能还送来最后一份
            if next.root == self.root {
                self.latest = Some(next);
                got = true;
            }
        }
        got
    }

    pub fn busy(&self) -> bool {
        match &self.latest {
            Some(f) => !f.done,
            None => true,
        }
    }

    pub fn view(&self) -> Option<&Found> {
        self.latest.as_ref()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// 收集阶段每多少个文件检查一次取消。
/// 太小会把原子量打成热点，太大界面就不动了。
const CHECK_EVERY: usize = 512;
/// 进度快照的最小间隔（毫秒）。只按条数发会让"几千个文件的大目录"卡住不动。
const SNAP_MS: u128 = 250;

fn run_scan(
    root: PathBuf,
    cache_file: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<Found>,
) {
    let mut out = Found::new(root.clone());
    // 根读不了 ⇒ 后面一定是"0 个文件、没有重复"。那句话必须说成错误，
    // 不能说成结论——否则界面把"我进不去"报成"这里很干净"。
    if !root.is_dir() {
        out.error = Some(format!("{} 不是一个能扫的目录", root.display()));
        out.done = true;
        let _ = tx.send(out);
        return;
    }

    let mut cache = match &cache_file {
        Some(f) => match fmnr_fs::fingerprint::HashCache::load_from(f) {
            Ok(c) => c,
            Err(e) => {
                // 缓存文件读不出来不是失败理由，是"这次当没记住"——
                // 但要说出来：静默丢掉用户显式打开的"记住"，下次他会以为白开了。
                out.skipped.push(Skipped {
                    path: f.clone(),
                    reason: format!("读指纹缓存失败，本次不记住：{e}"),
                });
                fmnr_fs::fingerprint::HashCache::ephemeral()
            }
        },
        None => fmnr_fs::fingerprint::HashCache::ephemeral(),
    };

    // 1) 先收文件名。整棵树的**路径**要留在内存里（哈希不留），
    //    一百万个路径约几十 MB，比留哈希或留内容都便宜。
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in jwalk::WalkDir::new(&root)
        .skip_hidden(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        files.push(entry.path());
        if files.len().is_multiple_of(CHECK_EVERY) && cancel.load(Ordering::Relaxed) {
            out.aborted = true;
            out.done = true;
            let _ = tx.send(out);
            return;
        }
    }
    out.candidates = files.len();

    // 2) 阶梯比对。进度回调同时负责取消与发快照。
    let started = Instant::now();
    let mut last_snap = started;
    let cb_cancel = Arc::clone(&cancel);
    // 刻意不写 `move`：闭包要**借用** out/tx（之后还要接着用它们收尾），
    // 吞掉所有权的话编译器直接把这条判死。
    let mut on_progress = |n: u64| -> bool {
        out.scanned = n;
        if cb_cancel.load(Ordering::Relaxed) {
            return false;
        }
        if last_snap.elapsed().as_millis() >= SNAP_MS {
            last_snap = Instant::now();
            let mut snap = out.clone();
            snap.done = false;
            if tx.send(snap).is_err() {
                return false;
            }
        }
        true
    };
    let report = dupes::find_duplicates_cached(
        &files,
        fmnr_fs::dupes::sha256_of,
        Some(&mut cache),
        Some(&mut on_progress),
    );

    out.groups = report.groups;
    out.skipped.extend(report.skipped);
    out.aborted = report.aborted || out.aborted;
    out.scanned = (out.candidates as u64).max(out.scanned);
    out.done = true;

    // 3) 记住才落盘。写失败要说出来，但不推翻结果——清单是真的算出来了。
    if let Some(f) = &cache_file {
        if let Err(e) = cache.save_to(f) {
            out.skipped.push(Skipped {
                path: f.clone(),
                reason: format!("指纹缓存没写进去：{e}"),
            });
        }
    }
    let _ = tx.send(out);
}

/// 缓存文件放哪儿（由调用方给配置目录，这里只定文件名）。
pub fn cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("fmnr").join("dup-hash-cache.txt")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_tree(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fmnr-dupes-view-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let blob = vec![7u8; 5000];
        std::fs::write(d.join("a.bin"), &blob).unwrap();
        std::fs::write(d.join("sub/b.bin"), &blob).unwrap();
        std::fs::write(d.join("only.bin"), b"unique content here").unwrap();
        d
    }

    fn wait(r: &mut Running) -> Found {
        let t0 = Instant::now();
        loop {
            r.poll();
            if let Some(f) = r.view() {
                if f.done {
                    return f.clone();
                }
            }
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(60),
                "60s 没扫完"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// 正例：跨子目录的重复要找到，且"可回收"按**每组留一个**算。
    #[test]
    fn finds_duplicates_across_subdirs_and_prices_the_saving() {
        let d = tmp_tree("hit");
        let mut r = Running::start(d.clone(), None);
        let got = wait(&mut r);
        assert_eq!(got.groups.len(), 1, "实得 {:?}", got.groups);
        assert_eq!(got.groups[0].paths.len(), 2);
        assert_eq!(got.reclaimable(), 5000, "两件 5000B 只该算省一份");
        assert!(!got.aborted);
        assert!(got.error.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 取消：结果必须带着 `aborted` 回来，而不是悄悄给一份看起来完整的清单。
    #[test]
    fn cancel_marks_the_report_aborted() {
        let d = tmp_tree("cancel");
        let r = Running::start(d.clone(), None);
        r.cancel();
        let mut r = r;
        let got = wait(&mut r);
        assert!(got.done);
        assert!(
            got.aborted || got.groups.is_empty(),
            "取消了却给出像完整的结论：aborted={:?} groups={:?}",
            got.aborted,
            got.groups.len()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// root 不存在：要说"扫不动"，不能显示成"没有重复"。
    #[test]
    fn missing_root_is_not_reported_as_no_duplicates() {
        let d =
            std::env::temp_dir().join(format!("fmnr-dupes-view-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut r = Running::start(d.clone(), None);
        let got = wait(&mut r);
        assert!(got.done);
        assert!(
            got.error.is_some(),
            "root 不存在必须给 error，不能让界面说\"查过 0 个文件，没有重复\"：{got:?}"
        );
    }

    /// 没给缓存文件就一个字节都不该往盘上写。
    #[test]
    fn no_cache_file_means_no_disk_write() {
        let d = tmp_tree("nocache");
        let probe =
            std::env::temp_dir().join(format!("fmnr-dupes-nocache-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&probe);
        let mut r = Running::start(d.clone(), None);
        let _ = wait(&mut r);
        assert!(!probe.exists(), "没开记住却写了缓存文件");
        let _ = std::fs::remove_dir_all(&d);
    }
}
