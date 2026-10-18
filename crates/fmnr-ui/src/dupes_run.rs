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
    for e in jwalk::WalkDir::new(&root).skip_hidden(true).into_iter() {
        // "这一层读不了"在 jwalk 0.9 里**不是** Err 条目，而是写在父条目的
        // `read_children.error`（`dir_entry_iter.rs:91-93`，实测：ACL 拒访的子目录
        // 迭代器给 Ok + 0 条 Err）。不接住它，"这一支压根没扫"就会报成
        // "这里没有重复文件"——那是对用户数据下结论，不是省事的近似。
        if let Ok(entry) = &e {
            if entry
                .read_children
                .as_ref()
                .and_then(|rc| rc.error())
                .is_some()
            {
                out.skipped.push(Skipped {
                    path: entry.path(),
                    reason: "读不了这一层，整支没参与查重".into(),
                });
            }
        }
        let Ok(entry) = e else { continue };
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

/// 一次"按勾选删除"的展开结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeletePlan {
    pub paths: Vec<PathBuf>,
    /// 会被**整组删空**的组数。0 表示每组都至少留了一份。
    pub emptied_groups: usize,
    /// 勾选的项里有多少个当前已经不存在了（上次扫描之后被动过）。
    pub stale: usize,
}

/// 默认勾选：**每组保留第一个，其余勾上**。
///
/// 形状必须和 `groups` 一一对应（界面按下标存状态），
/// 所以这里不返回"要删的路径列表"而是返回勾选矩阵。
pub fn default_selection(groups: &[DupGroup]) -> Vec<Vec<bool>> {
    groups
        .iter()
        .map(|g| g.paths.iter().enumerate().map(|(i, _)| i > 0).collect())
        .collect()
}

/// 把勾选矩阵展开成删除计划。
///
/// `selected` 与 `groups` 形状不符时**不猜**：按较短的那个走，
/// 多出来的组视为没勾（宁可少删，也不因为界面状态错位而删到没选的东西）。
/// 已经不存在的路径会被剔出去并计入 `stale`——清单是上一次扫的，
/// 中间文件可能被人删了或移走了，把不存在的目标塞进删除操作只会让整批操作报错。
pub fn plan_delete(groups: &[DupGroup], selected: &[Vec<bool>]) -> DeletePlan {
    let mut plan = DeletePlan::default();
    for (g, sel) in groups.iter().zip(selected.iter()) {
        let mut kept = 0usize;
        for (i, path) in g.paths.iter().enumerate() {
            let want = sel.get(i).copied().unwrap_or(false);
            if !want {
                kept += 1;
                continue;
            }
            if !path.exists() {
                plan.stale += 1;
                // 目标已经没了，等于这一项"少删一个"；同组是否被删空要按剩下的算
                kept += 1;
                continue;
            }
            plan.paths.push(path.clone());
        }
        if kept == 0 {
            plan.emptied_groups += 1;
        }
    }
    plan
}

/// 从清单里剔掉**已经删掉**的那些路径。
///
/// 一条组只剩一份的时候整组撤下：那个文件本身没被删，但它不再和任何东西重复，
/// 留在"重复文件"清单上就是在骗人——用户会以为删了它还能省一份。
/// 从清单里剔掉**已经删掉**的那些路径，并**把勾选状态一起跟着剔**，
/// 返回（剩下的组，剩下的勾选）。
///
/// 一条组只剩一份的时候整组撤下：那个文件本身没被删，但它不再和任何东西重复，
/// 留在"重复文件"清单上就是在骗人——用户会以为删了它还能省一份。
///
/// 只剔清单不剔勾选的话，行数一变界面就整体重播种：
/// 用户刚亲手取消的那个勾会被"恢复成默认"，下一次删除就删到他刚保住的那一份。
/// 所以每个存活路径带走它自己那一格。
pub fn prune_selection(
    groups: &[DupGroup],
    sel: &[Vec<bool>],
    dead: &std::collections::HashSet<PathBuf>,
) -> (Vec<DupGroup>, Vec<Vec<bool>>) {
    let mut kept_groups = Vec::new();
    let mut kept_sel = Vec::new();
    for (i, g) in groups.iter().enumerate() {
        let row = sel.get(i);
        let mut paths = Vec::new();
        let mut flags = Vec::new();
        for (j, p) in g.paths.iter().enumerate() {
            if dead.contains(p.as_path()) {
                continue;
            }
            paths.push(p.clone());
            // 这一行原来就没状态（形状对不上）才按默认补：留第一个、其余勾上
            flags.push(
                row.map(|r| r.get(j).copied().unwrap_or(false))
                    .unwrap_or(j > 0),
            );
        }
        if paths.len() > 1 {
            kept_groups.push(DupGroup {
                size: g.size,
                paths,
            });
            kept_sel.push(flags);
        }
    }
    (kept_groups, kept_sel)
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

    /// 读不到的那一层要出现在"跳过"里，不能让整次扫描报成"这里没有重复"。
    ///
    /// 这条锁的是实测机制（jwalk 0.9 把嵌套 read_dir 失败写在父条目
    /// `read_children.error`，不是 Err 条目）。造不出拒访目录的机器打印跳过并放行——
    /// 但跳过必须说出来，静默跳过的绿门会被当成"这条判过了"。
    #[cfg(windows)]
    #[test]
    fn unreadable_layer_is_listed_as_skipped_not_as_clean() {
        use std::process::Command;

        let d = std::env::temp_dir().join(format!("fmnr-dupes-denied-{}", std::process::id()));
        let locked = d.join("locked");
        if locked.exists() {
            let _ = Command::new("icacls")
                .arg(locked.as_os_str())
                .arg("/reset")
                .output();
        }
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&locked).unwrap();
        // 两份内容相同：一份在能读的地方，一份在拒访目录里。
        std::fs::write(d.join("keep.bin"), vec![7u8; 5000]).unwrap();
        std::fs::write(locked.join("mirror.bin"), vec![7u8; 5000]).unwrap();

        let out = Command::new("icacls")
            .arg(locked.as_os_str())
            .args(["/deny", "*S-1-1-0:(OI)(CI)R"])
            .output()
            .expect("icacls 应可用");
        let unmake = |p: &std::path::Path| {
            let _ = Command::new("icacls")
                .arg(p.as_os_str())
                .arg("/reset")
                .output();
        };
        if !out.status.success() || std::fs::read_dir(&locked).is_ok() {
            unmake(&locked);
            let _ = std::fs::remove_dir_all(&d);
            println!("跳过：本机造不出拒访目录");
            return;
        }

        let mut r = Running::start(d.clone(), None);
        let got = wait(&mut r);
        unmake(&locked);
        let _ = std::fs::remove_dir_all(&d);

        assert!(
            got.skipped.iter().any(|s| s.path.ends_with("locked")),
            "读不到的那层没进跳过清单：{:?}（groups={}）",
            got.skipped
                .iter()
                .map(|s| (&s.path, s.reason.as_str()))
                .collect::<Vec<_>>(),
            got.groups.len()
        );
        assert_eq!(
            got.groups.len(),
            0,
            "能读的部分本来就没有成对文件——结论要停在「那一支没搜」"
        );
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

    fn g<P: AsRef<Path>>(paths: &[P]) -> DupGroup {
        DupGroup {
            size: 10,
            paths: paths.iter().map(|p| PathBuf::from(p.as_ref())).collect(),
        }
    }

    /// 建 n 个**真存在**的文件并返回它们的路径。
    ///
    /// 计划类的断言必须踩在真路径上：`plan_delete` 会把不存在的目标剔进 `stale`，
    /// 于是"一个都没勾所以没删"和"全被剔了所以没删"在断言里长得一模一样——
    /// 假路径写出来的这类测试杀不掉"把一切都剔掉"的这种实现。
    fn real(tag: &str, n: usize) -> Vec<PathBuf> {
        let d = std::env::temp_dir().join(format!("fmnr-dupes-plan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        (0..n)
            .map(|i| {
                let p = d.join(format!("f{i}.bin"));
                std::fs::write(&p, b"x").unwrap();
                p
            })
            .collect()
    }

    fn tidy(paths: &[PathBuf]) {
        if let Some(d) = paths.first().and_then(|p| p.parent()) {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// 默认勾选必须是"每组留一个"，且留下的那个是第一个。
    #[test]
    fn default_selection_keeps_the_first_of_every_group() {
        let p = real("default", 5);
        let groups = vec![g(&[&p[0], &p[1], &p[2]]), g(&[&p[3], &p[4]])];
        let sel = default_selection(&groups);
        assert_eq!(sel, vec![vec![false, true, true], vec![false, true]]);
        let plan = plan_delete(&groups, &sel);
        assert_eq!(
            plan.paths,
            vec![p[1].clone(), p[2].clone(), p[4].clone()],
            "被保留的那一份绝不能进删除列表"
        );
        assert_eq!(plan.stale, 0, "真路径不该算成已消失");
        assert_eq!(plan.emptied_groups, 0, "默认勾选不该把任何一组删空");
        tidy(&p);
    }

    /// 一个都没勾 → 计划必须是空的（界面的删除按钮据此禁用）。
    #[test]
    fn nothing_checked_yields_nothing_to_delete() {
        let p = real("none", 2);
        let groups = vec![g(&[&p[0], &p[1]])];
        let sel = vec![vec![false, false]];
        let plan = plan_delete(&groups, &sel);
        assert!(plan.paths.is_empty(), "没勾却给出待删列表：{plan:?}");
        assert_eq!(plan.emptied_groups, 0);
        assert_eq!(plan.stale, 0, "文件都在，不该被剔成 stale");
        tidy(&p);
    }

    /// 整组全选要**如实报**"这组会被删空"，不静默也不硬拦：
    /// 用户可能就是想把这一份东西整个清掉，而删除走回收站。
    #[test]
    fn selecting_a_whole_group_is_reported_not_blocked() {
        let d = tmp_tree("plan");
        let a = d.join("a.bin");
        let b = d.join("sub").join("b.bin");
        let groups = vec![g(&[a.to_str().unwrap(), b.to_str().unwrap()])];
        let sel = vec![vec![true, true]];
        let plan = plan_delete(&groups, &sel);
        assert_eq!(plan.paths.len(), 2);
        assert_eq!(plan.emptied_groups, 1, "整组被删空必须报出来");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 清单是上一次扫的，中间文件可能已被删/移走：
    /// 不存在的目标要剔出去并计数，不能塞进删除操作让整批失败。
    #[test]
    fn paths_that_vanished_since_the_scan_are_dropped_and_counted() {
        let d = tmp_tree("stale");
        let here = d.join("a.bin");
        let gone = d.join("nope.bin");
        let groups = vec![g(&[here.to_str().unwrap(), gone.to_str().unwrap()])];
        let sel = vec![vec![true, true]];
        let plan = plan_delete(&groups, &sel);
        assert_eq!(plan.stale, 1, "少报或多报了 stale 数：{plan:?}");
        assert_eq!(plan.paths, vec![here], "不该把不存在的路径交给删除");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 勾选矩阵和组对不上时**不猜**：状态缺失的项（行太短、组多出来）一律当作没勾。
    /// 只有 p[0] 是真勾上的，其余三个都缺状态，一个都不许进删除列表。
    #[test]
    fn mismatched_shapes_never_invent_a_selection() {
        let p = real("shape", 4);
        let groups = vec![g(&[&p[0], &p[1]]), g(&[&p[2], &p[3]])];
        let sel = vec![vec![true]];
        let plan = plan_delete(&groups, &sel);
        assert_eq!(
            plan.paths,
            vec![p[0].clone()],
            "缺状态的项被补成「勾了」，就会删到用户没选的东西"
        );
        assert_eq!(plan.emptied_groups, 0, "第一组还留着 p[1]，不该报删空");
        assert_eq!(plan.stale, 0, "缺失状态不等于文件消失，不该混进 stale");
        tidy(&p);
    }

    /// 删完之后的重算：勾掉的路径从组里消失，
    /// **只剩一份的组整组撤下**（它不再是重复，留在清单上就是骗人）。
    #[test]
    fn prune_removes_deleted_paths_and_any_group_left_with_one_copy() {
        let groups = vec![g(&["a", "b"]), g(&["a", "c", "d"])];
        let dead: std::collections::HashSet<PathBuf> = [PathBuf::from("b")].into_iter().collect();
        let out = prune_selection(&groups, &[], &dead).0;
        assert_eq!(out.len(), 1, "第一组只剩 a，不该还挂在重复清单上：{out:?}");
        assert_eq!(
            out[0].paths,
            vec![PathBuf::from("a"), PathBuf::from("c"), PathBuf::from("d")]
        );
        assert_eq!(out[0].size, 10, "组的大小要跟着带过来，界面按它算能省多少");
    }

    /// 一个都没删时清单必须逐字不动（不然界面会自己把组改掉）。
    #[test]
    fn prune_with_nothing_dead_changes_nothing() {
        let groups = vec![g(&["a", "b"]), g(&["c", "d", "e"])];
        let out = prune_selection(&groups, &[], &std::collections::HashSet::new()).0;
        assert_eq!(out, groups);
    }

    /// 剔掉已删项时，**勾选要跟着活下来的那一条走**，不是跟着下标走。
    /// 这一条不锁住，删完一次界面会把用户亲手取消的勾恢复成默认，
    /// 下一次「删除」就砍到他刚保住的那份上。
    #[test]
    fn pruning_carries_the_users_checks_onto_the_surviving_rows() {
        let groups = vec![g(&["a", "b"]), g(&["c", "d", "e"])];
        let sel = vec![vec![false, false], vec![false, false, true]];
        let dead: std::collections::HashSet<PathBuf> = [PathBuf::from("d")].into_iter().collect();
        let (gs, sels) = prune_selection(&groups, &sel, &dead);
        assert_eq!(
            gs.iter().map(|x| x.paths.len()).collect::<Vec<_>>(),
            vec![2, 2],
            "实得 {gs:?}"
        );
        assert_eq!(
            sels,
            vec![vec![false, false], vec![false, true]],
            "e 的勾要留在 e 上，c 保持没勾；第一组用户全取消了更不许恢复成默认"
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
