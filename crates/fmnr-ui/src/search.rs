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
pub fn find_range(haystack: &str, needle: &str, case_sensitive: bool) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    if case_sensitive {
        return haystack.find(needle).map(|i| i..i + needle.len());
    }
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();
    // to_lowercase 对某些字符（ß 等）会改变长度，字节偏移可能失效，
    // 故按 char 计数后再用 char_indices 还原到字节位置。
    let hchars: Vec<char> = h.chars().collect();
    let nchars: Vec<char> = n.chars().collect();
    if nchars.len() > hchars.len() {
        return None;
    }
    for i in 0..=(hchars.len() - nchars.len()) {
        if hchars[i..i + nchars.len()] == nchars[..] {
            let start = haystack.char_indices().nth(i).map(|(b, _)| b)?;
            let end = haystack
                .char_indices()
                .nth(i + nchars.len())
                .map(|(b, _)| b)
                .unwrap_or(haystack.len());
            return Some(start..end);
        }
    }
    None
}

pub struct SearchState {
    pub hits: Vec<Hit>,
    pub elapsed_ms: u64,
    pub done: bool,
    /// 命中总数（可能大于 hits.len()，后者只收了已到达的部分）
    pub total: usize,
    rx: Receiver<Vec<Hit>>,
    started: Instant,
}

impl SearchState {
    /// 非阻塞拉取一批新结果。返回是否有新增。
    pub fn drain(&mut self) -> bool {
        let mut got = false;
        loop {
            match self.rx.try_recv() {
                Ok(batch) => {
                    self.total += batch.len();
                    self.hits.extend(batch);
                    got = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.done = true;
                    self.elapsed_ms = self.started.elapsed().as_millis() as u64;
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
                        .send(std::mem::replace(&mut batch, Vec::with_capacity(BATCH)))
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
                let _ = tx.send(batch);
            }
        })
        .expect("spawn search thread");

    SearchState {
        hits: Vec::new(),
        elapsed_ms: 0,
        done: false,
        total: 0,
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
        for _ in 0..200 {
            st.drain();
            if st.done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(st.done);
        assert_eq!(st.hits.len(), 1);
        assert_eq!(st.hits[0].name, "beta.txt");
        assert!(st.hits[0].path.ends_with("beta.txt"));
        std::fs::remove_dir_all(&root).ok();
    }
}
