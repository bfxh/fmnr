//! 即时搜索：当前目录子树，谓词过滤。
//! 后台线程遍历，主线程按「首屏 <500ms」口径分批渲染。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Instant;

/// 结果分批回传：首批立即给用户看，后续增量补。
/// 一次全量返回会让大目录的 UI 卡死，与 DESIGN.md §8 首屏口径冲突。
const BATCH: usize = 200;

/// 搜索条件
#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    pub root: PathBuf,
    /// 命中后是否继续下钻子目录（true = 文件名匹配即可）
    pub case_sensitive: bool,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 命中片段在 name 中的字符范围，用于高亮
    pub range: std::ops::Range<usize>,
}

/// 子串匹配，返回字符范围；不区分大小写时保留原文位置。
///
/// 判定实现在内核（`fmnr_core::search`），与 MCP/CLI 的有界搜索共用一份——
/// 否则「高亮命中」和「搜索命中」会给出不同答案。
pub use fmnr_core::search::find_range;

pub struct SearchState {
    pub hits: Vec<Hit>,
    pub elapsed_ms: u64,
    pub done: bool,
    /// 命中总数（可能大于 hits.len()，后者只收了已到达的部分）
    pub total: usize,
    /// 搜不动的原因（卷读不了、远程断了）。
    ///
    /// 必须有这个字段：没有它，"我搜不了"和"这里没有匹配"在界面上是同一句话，
    /// 而后者会把用户引去改关键词，前者才是要他去看挂载/网络。
    pub error: Option<String>,
    rx: Receiver<Chunk>,
    started: Instant,
}

/// 后台线程往回送的东西：一批命中，或者一个失败原因。
enum Chunk {
    Batch(Vec<Hit>),
    Failed(String),
}

impl SearchState {
    /// 非阻塞拉取一批新结果。返回是否有新增。
    pub fn drain(&mut self) -> bool {
        let mut got = false;
        loop {
            match self.rx.try_recv() {
                Ok(Chunk::Batch(batch)) => {
                    self.total += batch.len();
                    self.hits.extend(batch);
                    got = true;
                }
                Ok(Chunk::Failed(e)) => {
                    self.error = Some(e);
                    got = true;
                }
                Err(TryRecvError::Disconnected) => {
                    // elapsed_ms 只记第一次：drain() 每帧都跑，
                    // 无条件覆写的话"搜索耗时"会随窗口开着一直涨，
                    // 扫完 2.5s 的树十分钟后显示成 17s。
                    if !self.done {
                        self.done = true;
                        self.elapsed_ms = self.started.elapsed().as_millis() as u64;
                    }
                    break;
                }
                Err(TryRecvError::Empty) => {
                    // 通道还开着且暂时没数据：若已过首批阈值就记一次首屏时间。
                    if self.elapsed_ms == 0 && self.started.elapsed().as_millis() as u64 >= 500 {
                        self.elapsed_ms = self.started.elapsed().as_millis() as u64;
                    }
                    break;
                }
            }
        }
        got
    }
}

/// 起一个后台搜索。
pub fn spawn_search(query: Query) -> SearchState {
    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    let root = query.root.clone();
    let needle = query.text.trim().to_string();
    let case_sensitive = query.case_sensitive;

    std::thread::Builder::new()
        .name("fmnr-search".into())
        .spawn(move || {
            if needle.is_empty() {
                return;
            }
            let mut batch = Vec::with_capacity(BATCH);
            // skip_hidden：与默认列表行为一致，不在结果里混进隐藏目录
            let walker = jwalk::WalkDir::new(&root).skip_hidden(true).into_iter();
            for entry in walker.filter_map(|e| e.ok()) {
                if batch.len() >= BATCH
                    && tx
                        .send(Chunk::Batch(std::mem::replace(
                            &mut batch,
                            Vec::with_capacity(BATCH),
                        )))
                        .is_err()
                {
                    return;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Some(range) = find_range(&name, &needle, case_sensitive) else {
                    continue;
                };
                let meta = entry.metadata().ok();
                batch.push(Hit {
                    path: entry.path(),
                    is_dir: entry.file_type().is_dir(),
                    name,
                    size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    range,
                });
            }
            if !batch.is_empty() {
                let _ = tx.send(Chunk::Batch(batch));
            }
        })
        .expect("spawn search thread");

    SearchState {
        hits: Vec::new(),
        elapsed_ms: 0,
        done: false,
        total: 0,
        error: None,
        rx,
        started,
    }
}

/// 走**卷抽象**的搜索：归档卷、远程卷的子树只有卷自己看得见。
///
/// 为什么不能沿用上面那条 jwalk 路径：它遍历的是本地文件系统，
/// 而归档卷里的 `docs/readme.txt` 在本地根本不存在（它在一个 zip 里）。
/// 实测那条路会去遍历**进程的当前目录**——相对虚拟路径被当成相对本地路径解析，
/// 于是"包内搜不到"变成"把 CWD 的子树搜了一遍"，报出来的还是"没有匹配项"。
///
/// 代价与本地那条不同，说清楚：这里是逐层 `list()` 的广度遍历，
/// 每一层一次卷调用。归档卷的清单在内存里（中央目录），所以便宜；
/// 远程卷是真 IO，慢到什么程度**没在真实主机上量过**，所以只做单线程、
/// 支持随时被丢弃（接收端一断，线程下一个 `send` 就退出）。
pub fn spawn_volume_search(
    volume: std::sync::Arc<dyn fmnr_core::Volume>,
    root: PathBuf,
    needle: String,
    case_sensitive: bool,
) -> SearchState {
    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    let needle = needle.trim().to_string();

    std::thread::Builder::new()
        .name("fmnr-search-volume".into())
        .spawn(move || {
            if needle.is_empty() {
                return;
            }
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Chunk::Failed(format!("起不了异步运行时：{e}")));
                    return;
                }
            };
            let opts = fmnr_core::ListOpts {
                show_hidden: true,
                sort: fmnr_core::SortOrder::Name,
            };
            let mut queue = std::collections::VecDeque::new();
            queue.push_back(PathBuf::from(&root));
            let mut batch: Vec<Hit> = Vec::with_capacity(BATCH);
            while let Some(dir) = queue.pop_front() {
                let entries = match rt.block_on(volume.list(&fmnr_core::VPath(dir.clone()), &opts))
                {
                    Ok(e) => e,
                    Err(e) => {
                        // 根读不了要说出来；深处的单层失败跳过（权限/软链环），
                        // 否则一个坏目录就把整次搜索判死，用户看不到已经搜到的东西。
                        if dir == root {
                            let _ = tx.send(Chunk::Failed(format!("{}：{e}", root.display())));
                        }
                        continue;
                    }
                };
                for entry in entries {
                    if entry.is_dir {
                        queue.push_back(dir.join(&entry.name));
                    }
                    let Some(range) = find_range(&entry.name, &needle, case_sensitive) else {
                        continue;
                    };
                    batch.push(Hit {
                        path: dir.join(&entry.name),
                        name: entry.name,
                        is_dir: entry.is_dir,
                        size: entry.size.unwrap_or(0),
                        range,
                    });
                    if batch.len() >= BATCH
                        && tx
                            .send(Chunk::Batch(std::mem::replace(
                                &mut batch,
                                Vec::with_capacity(BATCH),
                            )))
                            .is_err()
                    {
                        return;
                    }
                }
            }
            if !batch.is_empty() {
                let _ = tx.send(Chunk::Batch(batch));
            }
        })
        .expect("spawn volume search thread");

    SearchState {
        hits: Vec::new(),
        elapsed_ms: 0,
        done: false,
        total: 0,
        error: None,
        rx,
        started,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_case_insensitive_finds_byte_range() {
        let r = find_range("Report.txt", "port", false).unwrap();
        assert_eq!(&"Report.txt"[r.clone()], "port");
    }

    #[test]
    fn case_sensitive_rejects_wrong_case() {
        assert!(find_range("Report.txt", "report", true).is_none());
        assert!(find_range("Report.txt", "Report", true).is_some());
    }

    /// 非 ASCII 目标：lowercase 后长度可能变，必须仍落在原文的字符边界上。
    #[test]
    fn multibyte_falls_back_to_original_boundaries() {
        let hay = "Ökobilanz_2024.xlsx";
        let r = find_range(hay, "bilanz", false).unwrap();
        assert_eq!(&hay[r], "bilanz");
    }

    #[test]
    fn empty_needle_is_no_match() {
        assert!(find_range("abc", "", false).is_none());
        assert!(find_range("abc", "  ", false).is_none());
    }

    #[test]
    fn needle_longer_than_haystack_is_no_match() {
        assert!(find_range("ab", "abc", false).is_none());
    }

    #[test]
    fn background_search_finds_nested_hits() {
        let root = std::env::temp_dir().join(format!("fmnr-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("alpha.txt"), b"1").unwrap();
        std::fs::write(root.join("sub/beta.txt"), b"2").unwrap();
        std::fs::write(root.join("sub/gamma.log"), b"3").unwrap();

        let mut st = spawn_search(Query {
            text: "beta".into(),
            root: root.clone(),
            case_sensitive: false,
        });
        // 轮询等待，但**除下来的总预算是宽松的**：workspace 级并行会把
        // 遍历线程挤到 1s 之外，那时失败的是机器不是代码。超时要给出
        // 已等待多久，否则现场只剩一句没信息的 assertion failed。
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while !st.done && Instant::now() < deadline {
            st.drain();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            st.done,
            "搜索线程未在 30s 内结束（可能被并行测试挤占，非代码缺陷）"
        );
        assert_eq!(st.hits.len(), 1);
        assert_eq!(st.hits[0].name, "beta.txt");
        assert!(st.hits[0].path.ends_with("beta.txt"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 走卷抽象的那条路：嵌套命中必须搜得到。
    #[test]
    fn volume_walk_search_finds_nested_hits() {
        let root = tmp_tree("vol");
        let vol = std::sync::Arc::new(fmnr_fs::LocalVolume::new(root.clone()));
        let mut st = spawn_volume_search(vol, root.clone(), "beta".into(), false);
        wait_done(&mut st);
        assert_eq!(st.hits.len(), 1, "实得 {:?}", st.hits);
        assert_eq!(st.hits[0].name, "beta.txt");
        assert_eq!(st.error, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 根读不了要说成"搜不动"，不能报 0 命中——
    /// "这里没有"和"我看不到"是两件事，后者才是要用户去处理的。
    #[test]
    fn volume_walk_search_says_so_when_the_root_is_unreadable() {
        let missing =
            std::env::temp_dir().join(format!("fmnr-volsearch-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let vol = std::sync::Arc::new(fmnr_fs::LocalVolume::new(missing.clone()));
        let mut st = spawn_volume_search(vol, missing.clone(), "anything".into(), false);
        wait_done(&mut st);
        assert!(st.hits.is_empty());
        assert!(
            st.error.is_some(),
            "根读不了却没有任何错误——界面上就会显示成\"没有匹配项\""
        );
    }

    fn tmp_tree(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("fmnr-volsearch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("alpha.txt"), b"1").unwrap();
        std::fs::write(root.join("sub/beta.txt"), b"2").unwrap();
        root
    }

    /// 轮询等到收尾。**预算给得宽**：workspace 级并行会把遍历线程挤慢，
    /// 那时失败的是机器不是代码；超时要把已等多久说出来。
    fn wait_done(st: &mut SearchState) {
        let t0 = Instant::now();
        while !st.done && t0.elapsed() < std::time::Duration::from_secs(30) {
            st.drain();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(st.done, "搜索未在 30s 内收尾（已等 {:?}）", t0.elapsed());
    }
}
