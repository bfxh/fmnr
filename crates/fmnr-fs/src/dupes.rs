//! 重复文件查找：阶梯式筛选 + **字节确认**。
//!
//! 阶梯是 `大小分组 → 部分读哈希 → 全文件哈希 → 字节比对`。前三级都只是
//! **筛掉不可能相同的**，最后一级才是判定。为什么必须有最后一级：
//! 哈希可被构造碰撞（MD5 Stevens 2009、SHA-1 SHAttered 2017），而"同哈希不同内容"
//! 一旦被判成重复，用户按这个列表删文件就是**删掉不相等的东西**。
//! jdupes 的手册把字节对比写成它的显式 failsafe，不是可选优化。
//!
//! 这一层**只报清单，不删任何东西**：删除走 `ops.rs` 的回收站路径，
//! 由调用方（UI）逐项勾选后发起。
//!
//! ## 还没有做：跨次运行的指纹缓存
//! 用户提过"重复文件可以缓存，可以选择不记住"。缓存的锚点必须是
//! `(卷序列号, FileIdInfo, mtime, size)` 四元组——只按 `mtime + size` 会漏
//! timestomp、FAT 的 2 秒粒度、以及同尺寸改写。而 Windows 上拿 FileIdInfo
//! 要 `GetFileInformationByHandle`（std 的 `MetadataExt` 不给卷号也不给文件索引）。
//! ⇒ 那是一个独立的接线单元。**这里刻意不留一个"先按 mtime+size 缓存"的半成品**：
//! 那种缓存会把该重算的判成不用重算，比没有缓存更糟。

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 部分读时取头尾各多少字节。太小会让"同大小同头部、正文不同"的文件
/// 全部掉到全文件哈希那一级（慢），太大则在筛选阶段就读了太多。
pub const PARTIAL_BYTES: u64 = 4096;

/// 一组互为重复的文件（至少两个成员；单件不成组，不报）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupGroup {
    pub size: u64,
    pub paths: Vec<PathBuf>,
}

/// 读文件时出错的条目：路径 + 原因。
///
/// **必须回传**而不是静默跳过。一个 200 GB 的树里读不了 3 个文件，
/// 如果清单看起来是"完整的、没有重复"，用户就会按它删。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct DupReport {
    pub groups: Vec<DupGroup>,
    pub skipped: Vec<Skipped>,
    /// 参与比对的候选文件数（进了大小分组的那批）
    pub candidates: usize,
}

/// 默认的哈希实现：SHA-256。
pub fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// 阶梯查找。
///
/// `hash` 做成参数**不是为了灵活**，是为了可判红：想验"字节比对这道门真的在挡"，
/// 就得能喂一个故意全碰撞的哈希（比如恒返回同一个值），此时正确的实现
/// 仍然只把**字节真相等**的文件成组。写死 sha256 的话，那条断言压根写不出来。
pub fn find_duplicates(paths: &[PathBuf], hash: impl Fn(&[u8]) -> [u8; 32]) -> DupReport {
    find_duplicates_cached(paths, hash, None)
}

/// 同上，但可以带一个按四元组缓存全文件哈希的 [`HashCache`](crate::fingerprint::HashCache)。
///
/// 缓存**只省重算，不改判定**：成不成组仍然由字节比对说了算。
pub fn find_duplicates_cached(
    paths: &[PathBuf],
    hash: impl Fn(&[u8]) -> [u8; 32],
    mut cache: Option<&mut crate::fingerprint::HashCache>,
) -> DupReport {
    let mut report = DupReport::default();

    // 第 1 级：按大小分组。大小不同的文件不可能字节相等，这一刀最便宜。
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for p in paths {
        match std::fs::metadata(p) {
            Ok(m) if m.is_file() => by_size.entry(m.len()).or_default().push(p.clone()),
            Ok(_) => {}
            Err(e) => report.skipped.push(Skipped {
                path: p.clone(),
                reason: format!("取元数据失败：{e}"),
            }),
        }
    }
    by_size.retain(|_, v| v.len() > 1);
    report.candidates = by_size.values().map(|v| v.len()).sum();

    for (size, group) in by_size {
        // 第 2 级：头尾部分读哈希。绝大多数不等对在这里就被分开了，
        // 不必把整个文件读进内存。
        let mut by_partial: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
        for p in group {
            match read_partial(&p, size) {
                Ok(buf) => {
                    let h = match &mut cache {
                        Some(c) => {
                            let fp = crate::fingerprint::fingerprint_of(&p).ok();
                            let stage = crate::fingerprint::Stage::Partial;
                            match fp.as_ref().filter(|f| f.cacheable()) {
                                Some(f) => match c.get(f, stage) {
                                    Some(hit) => hit,
                                    None => {
                                        let made = hash(&buf);
                                        c.put(f, stage, made);
                                        made
                                    }
                                },
                                None => hash(&buf),
                            }
                        }
                        None => hash(&buf),
                    };
                    by_partial.entry(h).or_default().push(p.clone());
                }
                Err(e) => report.skipped.push(Skipped {
                    path: p.clone(),
                    reason: format!("部分读失败：{e}"),
                }),
            }
        }
        by_partial.retain(|_, v| v.len() > 1);

        for (_, cand) in by_partial {
            // 第 3 级：全文件哈希。
            let mut by_full: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
            for p in cand {
                // 先取指纹，读完**再取一次比对**：两次一样才敢把哈希记在这个指纹上。
                // 少了这一步，一个在读取途中被改写的文件会留下一条
                // "新指纹 → 旧内容哈希"的缓存，下次扫描就把不同的内容判成相同。
                let before = crate::fingerprint::fingerprint_of(&p).ok();
                match read_all(&p) {
                    Ok(bytes) => {
                        let h = match (before.as_ref(), cache.as_mut()) {
                            (Some(fp), Some(c)) if fp.cacheable() => {
                                match c.get(fp, crate::fingerprint::Stage::Full) {
                                    Some(hit) => hit,
                                    None => {
                                        let made = hash(&bytes);
                                        if crate::fingerprint::fingerprint_of(&p)
                                            .ok()
                                            .is_some_and(|after| after == *fp)
                                        {
                                            c.put(fp, crate::fingerprint::Stage::Full, made);
                                        }
                                        made
                                    }
                                }
                            }
                            _ => hash(&bytes),
                        };
                        by_full.entry(h).or_default().push(p.clone());
                    }
                    Err(e) => report.skipped.push(Skipped {
                        path: p.clone(),
                        reason: format!("全量读失败：{e}"),
                    }),
                }
            }
            by_full.retain(|_, v| v.len() > 1);

            // 第 4 级：字节比对——**唯一算判定的一级**。
            for (_, same_hash) in by_full {
                match confirm_by_bytes(&same_hash) {
                    Ok(confirmed) => {
                        for g in confirmed {
                            report.groups.push(DupGroup { size, paths: g });
                        }
                    }
                    Err(e) => report.skipped.push(Skipped {
                        path: same_hash[0].clone(),
                        reason: format!("字节比对失败：{e}"),
                    }),
                }
            }
        }
    }

    // 顺序要稳定：调用方按行号选文件，结果乱序会让"勾的是哪一行"变成猜谜。
    report.groups.sort_by(|a, b| {
        a.size
            .cmp(&b.size)
            .then_with(|| a.paths.first().cmp(&b.paths.first()))
    });
    report
}

/// 头尾各取一半（文件不够长就整个读）。
fn read_partial(path: &Path, size: u64) -> std::io::Result<Vec<u8>> {
    let take = PARTIAL_BYTES.min(size / 2).max(1) as usize;
    let mut f = File::open(path)?;
    let mut head = vec![0u8; take.min(size as usize)];
    f.read_exact(&mut head)?;
    if size <= (take as u64 * 2) {
        // 小文件：头尾会重叠，直接整个读更清楚
        let mut all = Vec::with_capacity(size as usize);
        f.seek(SeekFrom::Start(0))?;
        f.read_to_end(&mut all)?;
        return Ok(all);
    }
    let mut tail = vec![0u8; take];
    f.seek(SeekFrom::End(-(take as i64)))?;
    f.read_exact(&mut tail)?;
    head.extend_from_slice(&tail);
    Ok(head)
}

fn read_all(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len() as usize;
    let mut buf = Vec::with_capacity(len);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// 字节比对：把"哈希相同"收敛成"内容相同"。
/// 两两比对够用——同一哈希组内的成员数通常很小，且这里正确性优先于省几次读。
fn confirm_by_bytes(paths: &[PathBuf]) -> std::io::Result<Vec<Vec<PathBuf>>> {
    let mut out: Vec<Vec<PathBuf>> = Vec::new();
    for p in paths {
        let bytes = read_all(p)?;
        let mut placed = false;
        for g in out.iter_mut() {
            let head = g[0].clone();
            if read_all(&head)? == bytes {
                g.push(p.clone());
                placed = true;
                break;
            }
        }
        if !placed {
            out.push(vec![p.clone()]);
        }
    }
    Ok(out.into_iter().filter(|g| g.len() > 1).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fmnr-dupes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// 正例：真重复要成组。
    #[test]
    fn identical_files_group_together() {
        let d = tmp("same");
        let a = put(&d, "a.bin", &vec![7u8; 5000]);
        let b = put(&d, "b.bin", &vec![7u8; 5000]);
        let _c = put(&d, "c.bin", &vec![9u8; 5000]);
        let r = find_duplicates(&[a.clone(), b.clone()], sha256_of);
        assert_eq!(r.groups.len(), 1, "实得 {:?}", r.groups);
        assert_eq!(r.groups[0].paths.len(), 2);
        assert_eq!(r.groups[0].size, 5000);
        assert!(r.skipped.is_empty(), "不该有跳过项：{:?}", r.skipped);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 🔴 核心判据：**同哈希不同内容不许判成重复**。
    /// 喂一个恒等哈希（所有输入都碰撞），字节比对那道门必须把不等的内容分开。
    /// 这条就是哈希做成参数的理由——写死 sha256 就造不出这个场景。
    #[test]
    fn colliding_hash_still_does_not_group_different_bytes() {
        let d = tmp("collide");
        // 同大小、同头尾、正文不同：部分读那一关也分不开，只能靠字节比对
        let mut x = vec![1u8; 20000];
        let y = vec![1u8; 20000];
        x[10000] = 2;
        let a = put(&d, "x.bin", &x);
        let b = put(&d, "y.bin", &y);
        let collide = |_: &[u8]| [0u8; 32];
        let r = find_duplicates(&[a, b], collide);
        assert!(
            r.groups.is_empty(),
            "碰撞哈希把不相等的文件判成了重复：{:?}",
            r.groups
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 同哈希且**真相等**时，碰撞哈希下仍然要成组（否则上面那条断言
    /// 可以由"永远不分组"实现——两合起来才封死偷懒的实现）。
    #[test]
    fn colliding_hash_still_groups_equal_bytes() {
        let d = tmp("collide-eq");
        let a = put(&d, "a.bin", b"same same same");
        let b = put(&d, "b.bin", b"same same same");
        let r = find_duplicates(&[a, b], |_| [0u8; 32]);
        assert_eq!(r.groups.len(), 1, "相等的内容必须成组");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 大小不同不可能字节相等：不许为了"多找几组"去读它们。
    #[test]
    fn different_sizes_never_group() {
        let d = tmp("size");
        let a = put(&d, "a.bin", b"short");
        let b = put(&d, "b.bin", b"much longer content");
        let r = find_duplicates(&[a, b], |_| [0u8; 32]);
        assert!(r.groups.is_empty());
        assert_eq!(r.candidates, 0, "大小不同就不该进候选池");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 读不到的文件必须**报出来**，不能静默少一项：
    /// 一份看起来完整、其实漏了三个文件的清单，用户会照着删。
    #[test]
    fn unreadable_paths_are_reported_not_swallowed() {
        let d = tmp("missing");
        let gone = d.join("nope.bin");
        let here = put(&d, "here.bin", b"x");
        let r = find_duplicates(&[gone.clone(), here], sha256_of);
        assert_eq!(
            r.skipped.len(),
            1,
            "读不到的那个必须出现在 skipped 里：{:?}",
            r.skipped
        );
        assert_eq!(r.skipped[0].path, gone);
        assert!(!r.skipped[0].reason.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 带上缓存之后：第二次扫描**不该再算一次哈希**，且**判定结果一模一样**。
    /// 两半都要断——只断"变快了"可以拿"永远返回同一个哈希"糊过去，
    /// 只断"结果相同"则可以拿"缓存根本没生效"糊过去。
    #[test]
    fn cache_avoids_rehashing_without_changing_the_verdict() {
        use std::cell::Cell;
        let d = tmp("cache");
        let a = put(&d, "a.bin", &vec![5u8; 9000]);
        let b = put(&d, "b.bin", &vec![5u8; 9000]);
        let mut c = crate::fingerprint::HashCache::persistent();
        let calls = Cell::new(0u32);
        let counting = |bytes: &[u8]| {
            calls.set(calls.get() + 1);
            sha256_of(bytes)
        };
        let r1 = find_duplicates_cached(&[a.clone(), b.clone()], counting, Some(&mut c));
        let after_first = calls.get();
        assert!(after_first > 0, "第一次扫描必须真算过哈希");
        let (hits1, misses1) = c.stats();
        // 两级都查缓存（部分读 + 全文件），2 个文件 ⇒ 4 次查询，全 miss
        assert_eq!((hits1, misses1), (0, 4), "第一次应当全是 miss");

        calls.set(0);
        let r2 = find_duplicates_cached(&[a, b], counting, Some(&mut c));
        assert_eq!(calls.get(), 0, "全命中时一次哈希都不该再算");
        assert_eq!(c.stats(), (4, 4), "第二次应当全是 hit");
        assert_eq!(r1.groups, r2.groups, "缓存不许改变判定结果");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 头尾相同、中间不同的两个文件不许被判成重复。
    ///
    /// ⚠ 我原本写这条是为了抓"部分读与全文件共用一个缓存槽"，**这个说法被
    /// 反证否掉了**：把阶段从缓存键里拿掉之后，这条测试**照样过**——因为
    /// 两级挤同一槽只会让全文件那一查拿到"部分读的短哈希"，两个文件因此
    /// 拿到同一个（错的）哈希而成组，接着**字节比对那一级把它们分开**。
    /// 真正咬住那个变异的是 `cache_avoids_rehashing_without_changing_the_verdict`
    /// 的命中/未命中计数（少一个槽位 ⇒ 第二次扫描出现 miss ⇒ 判红）。
    /// 所以这条的实际作用是：**即使缓存喂进来一个错哈希，字节门仍然兜住**
    /// ——那是纵深防御，不是槽位隔离的判据。
    #[test]
    fn partial_and_full_hashes_do_not_share_a_cache_slot() {
        let d = tmp("stage");
        let mut x = vec![1u8; 20000];
        let mut y = vec![1u8; 20000];
        // 只动中间（头尾 4096 之外），保证部分读看到的内容完全一致
        x[10000] = 2;
        y[10000] = 3;
        let a = put(&d, "x.bin", &x);
        let b = put(&d, "y.bin", &y);
        let mut c = crate::fingerprint::HashCache::persistent();
        let r = find_duplicates_cached(&[a, b], sha256_of, Some(&mut c));
        assert!(
            r.groups.is_empty(),
            "头尾相同、中间不同的两个文件被判成重复了：{:?}",
            r.groups
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 不带缓存（`None`）时行为与 `find_duplicates` 一致。
    #[test]
    fn no_cache_matches_the_plain_path() {
        let d = tmp("nocache");
        let a = put(&d, "a.bin", &vec![3u8; 7000]);
        let b = put(&d, "b.bin", &vec![3u8; 7000]);
        let plain = find_duplicates(&[a.clone(), b.clone()], sha256_of);
        let via_api = find_duplicates_cached(&[a, b], sha256_of, None);
        assert_eq!(plain.groups, via_api.groups);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 目录不是文件，不该进清单也不该报错（调用方常常整棵树丢进来）。
    #[test]
    fn directories_are_neither_grouped_nor_reported_as_errors() {
        let d = tmp("dir");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let r = find_duplicates(&[d.join("sub")], sha256_of);
        assert!(r.groups.is_empty());
        assert!(r.skipped.is_empty(), "目录不该算读取失败：{:?}", r.skipped);
        let _ = std::fs::remove_dir_all(&d);
    }
}
