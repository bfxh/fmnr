//! 磁盘占用的实时扫描。
//!
//! 内存口径是这里唯一真正的设计约束：**不保存整棵树**。
//! 一次扫描只留"当前这一层的每个子目录一个累计值"（O(子目录数)），
//! 文件在遍历过程中被折算进所属的那个桶，之后就丢掉。
//! 350 万条目那种树，保存整棵树要先于磁盘把内存吃光。
//!
//! "实时"的做法是**边扫边发累计快照**：UI 上的条会一路长，
//! 并在扫完之前明说"还在算"——半截数字看起来和结论一样，所以必须标出来。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

/// 当前层里的一个子目录累计到的量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub path: PathBuf,
    pub name: String,
    pub bytes: u64,
    pub entries: u64,
    /// 这一支里有读不了的目录（权限/重解析点）。不是 0，是"没算进去"。
    pub errors: u64,
}

/// 一次扫描的累计结果。
#[derive(Debug, Clone)]
pub struct Usage {
    pub root: PathBuf,
    /// 直接躺在 root 下的文件之和（不属于任何子目录的那部分）
    pub loose_bytes: u64,
    pub buckets: Vec<Bucket>,
    pub visited: u64,
    pub done: bool,
    /// 扫描本身失败（root 读不了）。和"扫完发现是空的"是两件事。
    pub error: Option<String>,
}

impl Usage {
    fn new(root: PathBuf) -> Self {
        Usage {
            root,
            loose_bytes: 0,
            buckets: Vec::new(),
            visited: 0,
            done: false,
            error: None,
        }
    }

    /// 按字节降序。占比条要"最大的在最上面"，否则用户先看到的是零头。
    pub fn sorted(&self) -> Vec<&Bucket> {
        let mut v: Vec<&Bucket> = self.buckets.iter().collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.bytes));
        v
    }

    /// 这一层总共算出来多少（含散落文件）。占比条的分母。
    pub fn total(&self) -> u64 {
        self.loose_bytes + self.buckets.iter().map(|b| b.bytes).sum::<u64>()
    }
}

/// `path` 属于 root 下哪一个直接子目录？`None` = 它就是 root 里的散落文件。
///
/// 这一步决定整张图的归属对不对，所以单独拎出来测。注意它**不**做大小写折叠：
/// jwalk 的回传路径是用我们递进去的 root 拼出来的，前缀必然一致；
/// 在这里加一层"不区分大小写的 strip"反而会掩盖传错 root 的 bug。
pub fn bucket_of(root: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(root).ok()?;
    let mut comps = rel.components();
    let first = comps.next()?;
    // 只剩一个组件 ⇒ 这个路径本身就是 root 的直接子项；
    // 对文件来说它就是散落文件，对目录来说不该走到这里（目录不计字节）。
    // 还有第二个组件 ⇒ 它在某个直接子目录里；只剩一个 ⇒ 它是这一层的散落项
    comps.next()?;
    Some(root.join(first.as_os_str()))
}

/// 在跑的扫描：拿 `cancel` 停，拿 `rx` 收快照。
pub struct Running {
    cancel: Arc<AtomicBool>,
    rx: Receiver<Usage>,
    /// `None` = 一条快照都还没收到。拿"空的 Usage"当占位是不行的：
    /// 那会被画成"这一层 0 个文件"，而真相是"还不知道"。
    latest: Option<Usage>,
    /// 这次扫描的根。取消旧扫描时，它可能还会送来最后一份快照——
    /// 那份的数属于**上一个目录**，必须按 root 丢掉，否则下钻之后
    /// 标题是新路径、条子是旧路径，那是最难查的一种串台。
    root: PathBuf,
}

impl Running {
    /// 起一次扫描。`root` 不存在时不起线程，直接给一个带 error 的结果——
    /// "起个线程去读一个不存在的路径"只会把错误推迟到更难归因的地方。
    pub fn start(root: PathBuf) -> Self {
        if !root.is_dir() {
            let mut u = Usage::new(root.clone());
            u.done = true;
            u.error = Some(format!("{} 不是一个能读的目录", root.display()));
            let (tx, rx) = channel();
            let _ = tx.send(u.clone());
            return Running {
                cancel: Arc::new(AtomicBool::new(false)),
                rx,
                latest: Some(u),
                root,
            };
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let flag = cancel.clone();
        // 闭包把 root 吃掉，所以这里先留一份给自己比对快照归属
        let my_root = root.clone();
        std::thread::spawn(move || {
            let mut u = Usage::new(root.clone());
            let mut map: std::collections::HashMap<PathBuf, Bucket> =
                std::collections::HashMap::new();
            // 发布节奏同时看条数和时间：只按条数的话，几千文件的目录要等攒够
            // 阈值才动一下，"实时"就成了"最后一次性出来"。
            let mut since_publish = 0u64;
            let mut last_publish = std::time::Instant::now();
            // 每个桶**读不到的目录个数**。用计数而不是集合：
            // 早先用 HashSet + 每次发布 `+= 1`，同一个失败目录会被后面每次发布
            // 反复加一，扫得越久"没读到的目录"越大——那是把计数器当成了增量日志。
            let mut err_dirs: std::collections::HashMap<PathBuf, u64> =
                std::collections::HashMap::new();
            let mut root_err: Option<String> = None;

            for msg in jwalk::WalkDir::new(&root)
                .parallelism(jwalk::Parallelism::Serial)
                .skip_hidden(false)
                .follow_links(false)
                .into_iter()
            {
                if flag.load(Ordering::Relaxed) {
                    // 取消：把已经算出来的部分交回去，并说明没扫完
                    u.buckets = map.into_values().collect();
                    u.done = true;
                    u.error = Some("已取消".into());
                    let _ = tx.send(u);
                    return;
                }
                match msg {
                    Err(e) => {
                        // 只有**根**读不了才走这条（嵌套层的失败不以 Err 条目报出来，
                        // 见下面 Ok 分支里 read_children 那段）。仍然要记，
                        // 别让它伪装成"这个目录是空的"。
                        match e.path().map(|x| x.to_path_buf()) {
                            Some(p) if p == root => root_err = Some(e.to_string()),
                            Some(p) => {
                                if let Some(b) = bucket_of(&root, &p) {
                                    *err_dirs.entry(b).or_insert(0) += 1;
                                }
                            }
                            // 报不出路径的错误（线程池忙）：记一条总的，不丢
                            None => {
                                root_err.get_or_insert_with(|| e.to_string());
                            }
                        }
                        continue;
                    }
                    Ok(entry) => {
                        let p = entry.path();
                        // 这一层读不了：jwalk 0.9 不把它变成 Err 条目，而是塞进
                        // **父条目**的 `read_children.error`（实测：ACL 拒访的子目录，
                        // 迭代器给 Ok 若干条 + Err 0 条，错误只在字段里）。
                        // 不读这个字段的话，那一支会被画成"这个目录是空的"。
                        if let Some(rc) = entry.read_children.as_ref() {
                            if rc.error().is_some() {
                                if let Some(b) = bucket_of(&root, &p) {
                                    *err_dirs.entry(b).or_insert(0) += 1;
                                }
                            }
                        }
                        if entry.file_type().is_dir() {
                            continue;
                        }
                        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                        u.visited += 1;
                        since_publish += 1;
                        match bucket_of(&root, &p) {
                            None => u.loose_bytes += size,
                            Some(key) => {
                                let b = map.entry(key.clone()).or_insert_with(|| Bucket {
                                    // 名字取**桶自己**（那一层的目录名），
                                    // 不是手上这个文件的文件名——后者会让
                                    // `big/sub/deep.bin` 把整支标成 "deep.bin"。
                                    name: key
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_default(),
                                    path: key,
                                    bytes: 0,
                                    entries: 0,
                                    errors: 0,
                                });
                                b.bytes += size;
                                b.entries += 1;
                            }
                        }
                        if since_publish >= 4000 || last_publish.elapsed().as_millis() >= 250 {
                            since_publish = 0;
                            last_publish = std::time::Instant::now();
                            u.buckets = map.values().cloned().collect();
                            for (k, n) in err_dirs.iter() {
                                if let Some(b) = map.get_mut(k) {
                                    b.errors = *n;
                                }
                            }
                            let _ = tx.send(u.clone());
                        }
                    }
                }
            }
            for (k, n) in err_dirs.iter() {
                if let Some(b) = map.get_mut(k) {
                    b.errors = *n;
                }
            }
            u.buckets = map.into_values().collect();
            u.done = true;
            u.error = root_err;
            let _ = tx.send(u);
        });
        Running {
            cancel,
            rx,
            latest: None,
            root: my_root,
        }
    }

    /// 收增量。返回 `true` 表示拿到了新快照（要催重绘）。
    pub fn poll(&mut self) -> bool {
        let mut got = false;
        while let Ok(u) = self.rx.try_recv() {
            if u.root != self.root {
                // 上一次扫描的收尾快照，丢掉
                continue;
            }
            let done = u.done;
            self.latest = Some(u);
            got = true;
            if done {
                break;
            }
        }
        got
    }

    /// 在途 = 起了但还没出终态。没收到任何快照也算在途——
    /// 拿 `latest.root` 之类的哨兵判"起没起"是这里原先的错法。
    pub fn busy(&self) -> bool {
        self.latest.as_ref().is_none_or(|u| !u.done)
    }

    pub fn view(&self) -> Option<&Usage> {
        self.latest.as_ref()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_paths_belong_to_their_top_level_child() {
        let root = Path::new("/data");
        assert_eq!(
            bucket_of(root, Path::new("/data/a/b/c.txt")),
            Some(PathBuf::from("/data/a"))
        );
        assert_eq!(
            bucket_of(root, Path::new("/data/a")),
            None,
            "root 的直接子项本身不算某个子目录里的东西"
        );
        assert_eq!(bucket_of(root, Path::new("/data/x.txt")), None);
    }

    #[test]
    fn paths_outside_the_root_are_not_binned() {
        // 传错 root 要表现为"没算进去"，而不是被塞进某个桶里
        assert_eq!(
            bucket_of(Path::new("/data"), Path::new("/other/a/b.txt")),
            None
        );
    }

    #[test]
    fn total_includes_loose_files() {
        let mut u = Usage::new(PathBuf::from("/data"));
        u.loose_bytes = 10;
        u.buckets.push(Bucket {
            path: PathBuf::from("/data/a"),
            name: "a".into(),
            bytes: 90,
            entries: 3,
            errors: 0,
        });
        assert_eq!(u.total(), 100);
        assert_eq!(u.sorted()[0].bytes, 90);
    }

    #[test]
    fn sorted_puts_the_biggest_first() {
        let mut u = Usage::new(PathBuf::from("/d"));
        for (n, b) in [("small", 5u64), ("big", 500), ("mid", 50)] {
            u.buckets.push(Bucket {
                path: PathBuf::from(n),
                name: n.into(),
                bytes: b,
                entries: 1,
                errors: 0,
            });
        }
        let names: Vec<&str> = u.sorted().iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["big", "mid", "small"]);
    }

    /// 不存在的路径：不起线程、直接给一条 error。
    /// 这条锁的是"读不到"和"空的"不被混为一谈。
    #[test]
    fn missing_root_reports_an_error_instead_of_an_empty_scan() {
        let ghost = std::env::temp_dir().join("fmnr-usage-nope-7c1b");
        let mut r = Running::start(ghost);
        assert!(r.poll());
        let u = r.view().expect("该有一条带错的结果");
        assert!(u.done);
        assert!(u.error.is_some(), "error 必须说出来");
        assert_eq!(u.buckets.len(), 0);
        assert!(!r.busy());
    }

    /// 真扫一个临时目录树：验证 jwalk 那条路真能把字节归到正确的桶。
    ///
    /// 其中 `top.len() == 2` 那一条同时钉住 treemap 的内存边界（docs/UI-REWORK.md §4）：
    /// 这棵树里有 3 个目录，但只有 root 的**直接**子目录成桶。整棵树常驻是
    /// `Node` 88 B × 条目数，340 万条目 ≈ 299 MB（docs/UI-NEXT.md §3.3 实测），
    /// 所以"只保留当前层"不是省事的画法，是这张图能开的前提。
    #[test]
    fn scans_a_real_tree_and_bins_it() {
        let root = std::env::temp_dir().join("fmnr-usage-test-7c1b");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("big/sub")).unwrap();
        std::fs::create_dir_all(root.join("small")).unwrap();
        std::fs::write(root.join("loose.bin"), vec![7u8; 100]).unwrap();
        std::fs::write(root.join("big/sub/deep.bin"), vec![0u8; 900]).unwrap();
        std::fs::write(root.join("small/tiny.bin"), vec![0u8; 10]).unwrap();

        let mut r = Running::start(root.clone());
        // 扫描是后台线程，轮询到有终态为止（有界，不无限等）
        for _ in 0..200 {
            if r.poll() && r.view().is_some_and(|u| u.done) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let u = r.view().expect("至少收到一条快照").clone();
        assert!(u.done, "扫描该在 5 秒内收尾");
        assert_eq!(u.error, None, "扫描不该带错：{:?}", u.error);
        assert_eq!(u.loose_bytes, 100, "散落文件不计入任何子目录");
        let top = u.sorted();
        assert_eq!(
            top.len(),
            2,
            "两个子目录：{:?}",
            top.iter().map(|b| &b.name).collect::<Vec<_>>()
        );
        assert_eq!(top[0].name, "big");
        assert_eq!(top[0].bytes, 900, "子目录深处的文件要算到顶层那一支上");
        assert_eq!(top[1].bytes, 10);
        assert_eq!(u.total(), 1010);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 拒访的子目录**必须报成"有 N 个目录没读到"**，不能画成"这个目录是空的"。
    ///
    /// 这条锁的是实测出来的机制：jwalk 0.9 不把嵌套 `read_dir` 失败变成 Err 条目，
    /// 而是写进父条目的 `read_children.error`（`dir_entry_iter.rs:91-93`）。
    /// 只处理 `Err(_)` 分支的写法在这类目录上永不触发——那一支的字节会整个不见，
    /// 而图上看不出差别。
    #[cfg(windows)]
    #[test]
    fn unreadable_subdir_is_counted_instead_of_looking_empty() {
        use std::process::Command;

        let root = std::env::temp_dir().join(format!("fmnr-usage-denied-{}", std::process::id()));
        let reset = |locked: &std::path::Path| {
            if locked.exists() {
                let _ = Command::new("icacls")
                    .arg(locked.as_os_str())
                    .arg("/reset")
                    .output();
            }
        };
        reset(&root.join("branch").join("locked"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("branch").join("locked")).unwrap();
        std::fs::write(root.join("branch").join("visible.bin"), vec![3u8; 500]).unwrap();
        std::fs::write(
            root.join("branch").join("locked").join("hidden.bin"),
            vec![9u8; 4000],
        )
        .unwrap();

        let out = Command::new("icacls")
            .arg(root.join("branch").join("locked").as_os_str())
            .args(["/deny", "*S-1-1-0:(OI)(CI)R"])
            .output()
            .expect("icacls 应可用");
        if !out.status.success() || std::fs::read_dir(root.join("branch").join("locked")).is_ok() {
            // 这台机器造不出拒访目录（域策略/非 NTFS）：跳过并说明，
            // 不让 CI 因环境永久判红——本机实测这一步是能落地的。
            reset(&root.join("branch").join("locked"));
            let _ = std::fs::remove_dir_all(&root);
            println!("跳过：无法在本机造出拒访目录，这条判据需要能落 DACL 的卷");
            return;
        }

        let mut r = Running::start(root.clone());
        for _ in 0..200 {
            if r.poll() && r.view().is_some_and(|u| u.done) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let u = r.view().expect("至少收到一条快照").clone();
        reset(&root.join("branch").join("locked"));
        let _ = std::fs::remove_dir_all(&root);

        let branch = u
            .sorted()
            .into_iter()
            .find(|b| b.name == "branch")
            .expect("branch 这一支该在图里");
        assert!(
            branch.errors > 0,
            "读不到的子目录没被记账：这一支的 {}/4500 字节其实没算到",
            branch.bytes
        );
        assert_eq!(
            branch.bytes, 500,
            "能读的部分要照实算，读不到的部分不能凭空出现"
        );
    }
}
