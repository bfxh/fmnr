//! 文件指纹：`(卷序列号, 文件索引, mtime, 大小)` 四元组，以及按它做键的哈希缓存。
//!
//! 为什么四元组一个都不能少（这是 `docs/NEXT-BATCH.md` §4.4 定的锚点，不是随手加的）：
//! - **只按 `mtime + size`** 会漏三种真实情况：timestomp（工具可以任意设 mtime）、
//!   FAT/exFAT 的 **2 秒粒度**（同一秒内改完又改回来）、以及"换成另一份同尺寸内容"；
//! - **只按路径** 更不行：文件被移动、被另一个文件替换，路径完全不变。
//! - `卷号 + 文件索引` 才是"同一个文件对象"的身份：Windows 上
//!   `GetFileInformationByHandle` 给 `dwVolumeSerialNumber` 与 `nFileIndexHigh/Low`
//!   （✅ 已在 windows 0.62.2 本地源码核对字段类型，都是 `u32`），
//!   Unix 上对应 `MetadataExt` 的 `dev()`/`ino()`。
//!
//! 缓存**只省重算，不改判定**：重复文件最终的成组与否仍然由 `dupes` 的字节比对决定。
//! "可以选择不记住"是真的有开关（`HashCache::ephemeral()`），默认就是**不记**——
//! 记到盘上就要回答"什么时候失效"，而那个答案的正确性依赖上面四元组全都取得到。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// 一个文件的身份 + 内容指纹。四元组，全部参与相等判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint {
    /// 卷序列号（Unix 上是 `dev`）。
    pub volume: u64,
    /// 文件索引（Unix 上是 `ino`；Windows 上由 High/Low 两个 u32 拼成）。
    pub file_id: u128,
    /// mtime，纳秒。**用 `Option`**：拿不到（比如早于 1970 的时间）时
    /// 宁可当成"这条不能缓存"，也不要拿 0 冒充一个真实时间。
    pub mtime_ns: Option<i128>,
    pub size: u64,
}

impl Fingerprint {
    /// 能不能拿它当缓存键。mtime 缺失时返回 false ⇒ 调用方必须重算。
    pub fn cacheable(&self) -> bool {
        self.mtime_ns.is_some()
    }

    /// 稳定文本键（缓存文件里的一行）。
    ///
    /// `stage` 也要进键：部分读哈希和全文件哈希是**两个不同的函数**，
    /// 共用一个槽位会让"部分读的短哈希"被当成全文件哈希用。
    ///
    /// 四个分量**全部**进键。少编一个（比如只编 volume/file_id/mtime）
    /// 就意味着"同身份同 mtime 但大小不同"会命中旧哈希——
    /// 而大小恰恰是替换文件时最容易先动的那个。
    fn key_line(&self, stage: Stage) -> Option<String> {
        let t = self.mtime_ns?;
        Some(format!(
            "{:x}:{:x}:{:x}:{:x}:{:x}:{:?}",
            self.volume,
            self.file_id as u64,
            (self.file_id >> 64) as u64,
            t,
            self.size,
            stage
        ))
    }
}

/// 取一个文件的四元组指纹。
pub fn fingerprint_of(path: &Path) -> std::io::Result<Fingerprint> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("不是普通文件：{}", path.display()),
        ));
    }
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128);
    #[cfg(windows)]
    {
        let (volume, file_id) = win_identity(path)?;
        Ok(Fingerprint {
            volume,
            file_id,
            mtime_ns,
            size: meta.len(),
        })
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Fingerprint {
            volume: meta.dev(),
            file_id: meta.ino() as u128,
            mtime_ns,
            size: meta.len(),
        })
    }
}

#[cfg(windows)]
fn win_identity(path: &Path) -> std::io::Result<(u64, u128)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let file = std::fs::File::open(path)?;
    let handle = HANDLE(file.as_raw_handle() as *mut _);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // 这个 API 只吃句柄，不重开文件：句柄来自 `File::open`，
    // 所以拿到的卷号/索引与刚才 `metadata()` 描述的是同一个对象。
    unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(std::io::Error::other)?;
    let volume = info.dwVolumeSerialNumber as u64;
    let file_id = ((info.nFileIndexHigh as u128) << 64) | info.nFileIndexLow as u128;
    Ok((volume, file_id))
}

/// 缓存的是哪一级的哈希。两级都用同一个指纹做键，靠这个区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// 头尾部分读（`dupes` 的第 2 级）
    Partial,
    /// 全文件（第 3 级）
    Full,
}

/// 按四元组缓存两级的哈希。默认不持久化。
#[derive(Debug, Default)]
pub struct HashCache {
    map: HashMap<String, [u8; 32]>,
    /// false = 只在这次扫描内用（`ephemeral`）；true = 允许 `save_to` 落盘。
    remember: bool,
    hits: usize,
    misses: usize,
}

impl HashCache {
    /// 不记住：进程内缓存，扫描结束就没了。默认走这条。
    pub fn ephemeral() -> Self {
        Self {
            map: HashMap::new(),
            remember: false,
            hits: 0,
            misses: 0,
        }
    }

    /// 记住：允许落盘，下次 `load_from` 回来。
    pub fn persistent() -> Self {
        Self {
            remember: true,
            ..Self::ephemeral()
        }
    }

    pub fn remembering(&self) -> bool {
        self.remember
    }

    pub fn get(&mut self, fp: &Fingerprint, stage: Stage) -> Option<[u8; 32]> {
        let key = fp.key_line(stage)?;
        match self.map.get(&key) {
            Some(h) => {
                self.hits += 1;
                Some(*h)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    pub fn put(&mut self, fp: &Fingerprint, stage: Stage, hash: [u8; 32]) {
        // mtime 拿不到 ⇒ 四元组不完整 ⇒ 存了就可能把该重算的判成不用重算。
        let Some(key) = fp.key_line(stage) else {
            return;
        };
        self.map.insert(key, hash);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 空缓存是正常初始态，但 `clippy` 要求 `len` 配一个 `is_empty`。
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn stats(&self) -> (usize, usize) {
        (self.hits, self.misses)
    }

    /// 落盘。格式是每行 `<key>\t<hash_hex>`——刻意用文本而不是 JSON：
    /// 没有新增依赖，坏了也能一眼看出来是哪行。
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if !self.remember {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "这个缓存是 ephemeral 的：不记住就不该往盘上写",
            ));
        }
        let mut body = String::new();
        for (k, h) in &self.map {
            let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
            body.push_str(&format!("{k}\t{hex}\n"));
        }
        std::fs::write(path, body)
    }

    /// 读回。损坏的行（字段数不对、hex 长度不对）**跳过并计数**，
    /// 不整个失败——缓存丢几条的代价是重算，不是报错。
    pub fn load_from(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut c = Self::persistent();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Some((k, hex)) = line.split_once('\t') else {
                continue;
            };
            if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let mut h = [0u8; 32];
            for (i, b) in h.iter_mut().enumerate() {
                *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
            }
            c.map.insert(k.to_string(), h);
        }
        Ok(c)
    }
}

/// 缓存文件默认放哪儿（调用方给目录，这里只负责文件名）。
pub fn default_cache_file(dir: &Path) -> PathBuf {
    dir.join("fmnr-dup-hash-cache.txt")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(volume: u64, id: u64, mtime: i128, size: u64) -> Fingerprint {
        Fingerprint {
            volume,
            file_id: id as u128,
            mtime_ns: Some(mtime),
            size,
        }
    }

    /// 🔴 核心判据：**四元组任一分量变了，缓存必须不命中**。
    /// 这条把"少用一个分量"的实现钉死——比如只按 (mtime,size) 建键，
    /// 换文件索引就会命中旧哈希，正是 §4.4 说的 timestomp/替换文件那类漏。
    #[test]
    fn any_of_the_four_components_changing_invalidates_the_entry() {
        let base = fp(0x1111, 0x2222, 1_700_000_000_000_000_000, 4096);
        let hash = [7u8; 32];
        let variants = [
            base,
            fp(
                base.volume + 1,
                base.file_id as u64,
                1_700_000_000_000_000_000,
                4096,
            ),
            fp(
                base.volume,
                (base.file_id as u64) + 1,
                1_700_000_000_000_000_000,
                4096,
            ),
            fp(
                base.volume,
                base.file_id as u64,
                1_700_000_000_000_000_001,
                4096,
            ),
            fp(
                base.volume,
                base.file_id as u64,
                1_700_000_000_000_000_000,
                4097,
            ),
        ];
        let mut c = HashCache::persistent();
        c.put(&base, Stage::Full, hash);
        assert_eq!(c.get(&base, Stage::Full), Some(hash), "原指纹必须命中自己");
        for (i, v) in variants.iter().enumerate().skip(1) {
            assert_eq!(
                c.get(v, Stage::Full),
                None,
                "第 {i} 个分量变了还命中，说明缓存键少用了分量"
            );
        }
    }

    /// "可以选择不记住"必须是真的：ephemeral 缓存拒绝落盘。
    #[test]
    fn ephemeral_refuses_to_touch_the_disk() {
        let d = std::env::temp_dir().join(format!("fmnr-fpc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = default_cache_file(&d);
        let _ = std::fs::remove_file(&f);
        let mut c = HashCache::ephemeral();
        assert!(!c.remembering());
        c.put(&fp(1, 2, 3, 4), Stage::Full, [0u8; 32]);
        let e = c
            .save_to(&f)
            .expect_err("不记住的缓存不该允许写盘，实得 Ok");
        assert!(e.to_string().contains("不记住"), "文案要说清原因：{e}");
        assert!(!f.exists(), "拒绝之后也不该留下半个文件");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 落盘→读回必须等价（否则"记住"只是把数据扔进一个读不出来的文件）。
    #[test]
    fn persistent_cache_roundtrips() {
        let d = std::env::temp_dir().join(format!("fmnr-fpc-rt-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = default_cache_file(&d);
        let base = fp(0xaa, 0xbb, 123, 9);
        let mut c = HashCache::persistent();
        c.put(&base, Stage::Full, [3u8; 32]);
        c.save_to(&f).unwrap();
        let back = HashCache::load_from(&f).unwrap();
        let mut back = back;
        assert_eq!(
            back.get(&base, Stage::Full),
            Some([3u8; 32]),
            "读回后原条目必须还在"
        );
        assert_eq!(
            back.get(&fp(0xaa, 0xbb, 124, 9), Stage::Full),
            None,
            "读回后仍然要按四元组判"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 缓存文件被写坏时：跳过坏行、保留好行，不整个报错。
    #[test]
    fn corrupt_lines_are_skipped_not_fatal() {
        let d = std::env::temp_dir().join(format!("fmnr-fpc-bad-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = default_cache_file(&d);
        let base = fp(5, 6, 7, 8);
        let key = base.key_line(Stage::Full).unwrap();
        let good_hash: String = [9u8; 32].iter().map(|b| format!("{b:02x}")).collect();
        std::fs::write(
            &f,
            format!("no-tab-here\n{key}\tZZZZ\n{key}\t{good_hash}\n"),
        )
        .unwrap();
        let mut c = HashCache::load_from(&f).unwrap();
        assert_eq!(c.len(), 1, "只该收下那一行合法的，实得 {:?}", c.len());
        assert_eq!(c.get(&base, Stage::Full), Some([9u8; 32]));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// mtime 拿不到 ⇒ 不许当缓存键（宁可重算，也不拿 0 冒充一个真实时间）。
    #[test]
    fn fingerprint_without_mtime_is_not_cacheable() {
        let no_time = Fingerprint {
            volume: 1,
            file_id: 2,
            mtime_ns: None,
            size: 3,
        };
        assert!(!no_time.cacheable());
        let mut c = HashCache::persistent();
        c.put(&no_time, Stage::Full, [1u8; 32]);
        assert_eq!(c.len(), 0, "四元组不完整的条目不该被存进去");
        assert_eq!(c.get(&no_time, Stage::Full), None);
    }

    /// 真机取一次四元组：卷号与文件索引都必须**非零**（拿到 0 通常意味着
    /// API 没真调到，而不是这台机器特殊）。
    #[test]
    fn live_fingerprint_has_nonzero_identity() {
        let d = std::env::temp_dir().join(format!("fmnr-fpc-live-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("x.bin");
        std::fs::write(&f, b"hello").unwrap();
        let got = fingerprint_of(&f).unwrap();
        assert_eq!(got.size, 5);
        assert!(got.mtime_ns.is_some(), "mtime 应当拿得到");
        assert_ne!(got.file_id, 0, "文件索引为 0 ⇒ 身份没真取到：{got:?}");
        // 同一个文件再问一次必须给同样的身份
        assert_eq!(fingerprint_of(&f).unwrap(), got);
        // 改内容 ⇒ size 或 mtime 至少一个变（四元组因此不会把新内容当旧的）
        std::fs::write(&f, b"hello!").unwrap();
        let after = fingerprint_of(&f).unwrap();
        assert_ne!(after, got, "内容变了四元组却没动：{got:?} vs {after:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 目录不是文件：要报错，不能给一个假身份。
    #[test]
    fn directories_are_rejected() {
        let e = fingerprint_of(std::env::temp_dir().as_path()).expect_err("目录该被拒");
        assert!(e.to_string().contains("不是普通文件"), "文案要说清：{e}");
    }
}
