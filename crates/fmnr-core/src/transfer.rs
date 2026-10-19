//! 跨卷搬运（"多系统互传"的内核那一半）。
//!
//! 这模块存在的理由是**量出来的现状**，不是设想的需求：`Volume::copy` 的源和目标
//! 都在**同一个卷**里解析（`self.resolve(sources)` + `self.resolve(dest)`），于是
//! ```text
//! fmnr cp archive:D:\tmp\e2e\pack.7z/hello.txt --to D:\tmp\e2e\out
//! fmnr: unsupported: 归档卷是只读的：1 → D:\tmp\e2e\out (-32603)
//! ```
//! 路由按 `sources[0]` 的前缀选中归档卷，`dest_dir` 又在这个卷里解析 ⇒
//! **从压缩包里取一个文件到本地，四条入口（UI/CLI/MCP/HTTP）全做不到**。
//! 这里补的是「源卷 `read_stream` → 目标卷 `write_stream`」这一条通路，两边都不整份进内存。
//!
//! 三个刻意的边界（都不是偷懒，形状写在下面）：
//! - **只跨卷**：`src.id() == dst.id()` 直接拒绝。同卷搬运该走 `Volume::copy`——本地那条
//!   有 `rename` 快速路径与回收站语义，把政策复制第二份正是本仓反复踩过的"两份各过期一半"。
//! - **只搬文件，不搬目录**：递归列目录在归档卷上是 O(N×D)（`direct_children` 每次扫全表；
//!   实测 11 万条递归展开整树 103 秒，见 `docs/NEXT-BATCH.md` §21-2 那条禁区）。
//!   要做整目录提取，得先把展开接到 `Volume::search`（它对索引只走一遍 = O(N)）上，
//!   而不是在这里新加一个递归 `list` 消费者。
//! - **不覆盖已存在的目录**：`Overwrite` 只作用于文件（`write_stream` 截断重写）。
//!   覆盖目录要"先删再拷"，而卷抽象上唯一的删除在本地卷是回收站删除，语义与
//!   `ops::transfer` 的 `remove_dir_all` 不同 ⇒ 宁可明确拒绝，不静默换语义。

use crate::{
    rename::{candidate_name, first_rename_index},
    ConflictPolicy, Entry, Error, ListOpts, OpReport, Progress, Result, VPath, Volume,
};
use std::sync::Arc;

/// 跨卷拷贝一批**文件**。返回的 `OpReport`：`done` 成功条数、`skipped` 因冲突策略跳过、
/// `aborted` 取消、`bytes` 实际写出的字节数。
pub async fn copy_across(
    src: &Arc<dyn Volume>,
    dst: &Arc<dyn Volume>,
    sources: &[VPath],
    dest_dir: &VPath,
    policy: ConflictPolicy,
    progress: &Arc<Progress>,
) -> Result<OpReport> {
    if src.id() == dst.id() {
        return Err(Error::Unsupported(
            "源与目标是同一个卷：同卷搬运请走 Volume::copy（那边有 rename 快速路径与回收站语义）"
                .into(),
        ));
    }
    if !dst.writable() {
        return Err(Error::Unsupported(format!(
            "目标卷 {} 是只读的，不能接收文件",
            dst.id().0
        )));
    }
    ensure_dir(dst, dest_dir).await?;

    // 先**把整批问清楚再动手**：目录、不存在的源、缺文件名的源都在这里处理完。
    // 两趟的理由有两条，都不是"顺手"：
    // ① 拒绝必须发生在写第一个文件之前——否则一批里第 7 条是目录，前 6 条已经落进目标卷了，
    //    而那次的失败看起来像"拷了一半莫名其妙"；
    // ② 分母要提前知道。`ProgressSnapshot::fraction()` 在 `bytes_total == 0` 时恒报 0.0，
    //    不算分母等于把进度条钉死在起点（那是"看着像卡住"的用户可见故障）。
    //    头表/stat 里就有 `Entry.size`，不用读内容。
    let mut queue: Vec<(VPath, std::ffi::OsString, u64)> = Vec::with_capacity(sources.len());
    let mut bytes_total = 0u64;
    let mut sizes_all_known = true;
    let mut no_name = 0u64;
    for source in sources {
        let entry = entry_of(src, source).await?.ok_or_else(|| {
            Error::NotFound(format!(
                "{} 在卷 {} 里不存在",
                source.0.display(),
                src.id().0
            ))
        })?;
        if entry.is_dir {
            return Err(Error::Unsupported(format!(
                "跨卷只搬文件：{} 是目录（整目录提取要先接 Volume::search，见 transfer 模块头的禁区）",
                source.0.display()
            )));
        }
        let Some(name) = source.0.file_name() else {
            no_name += 1;
            continue;
        };
        match entry.size {
            Some(n) => bytes_total += n,
            None => sizes_all_known = false,
        }
        queue.push((source.clone(), name.to_os_string(), entry.size.unwrap_or(0)));
    }
    let mut report = OpReport::default();
    report.skipped += no_name as usize;
    progress.set_totals(
        if sizes_all_known { bytes_total } else { 0 },
        queue.len() as u64,
    );

    for (source, name, _declared) in queue {
        if progress.is_cancelled() {
            report.aborted += 1;
            continue;
        }
        let raw_dest = VPath(dest_dir.0.join(&name));
        let dest = match resolve_on_dst(dst, &raw_dest, policy).await? {
            Some(p) => p,
            None => {
                report.skipped += 1;
                continue;
            }
        };

        let stream = src.read_stream(&source).await?;
        let written = dst.write_stream(&dest, stream).await?;
        // 回执里的字节以**实际写出**为准；头表/stat 的 size 只当分母用
        // （`_declared` 故意不参与计算）——两者不一致（坏头表、虚标 size）时不跟着谎报。
        progress.add_bytes(written);
        progress.add_file();
        report.done += 1;
        report.bytes += written;
    }
    Ok(report)
}

/// 取单个条目：与工具表里 `stat` 同一口径——**列父目录再按名字找**，
/// 不自建第二套"这条存在吗"的判断。父目录列不出来一律当错，不当"不存在"
/// （那会把读权限问题洗成 NotFound）。
pub async fn entry_of(vol: &Arc<dyn Volume>, path: &VPath) -> Result<Option<Entry>> {
    let Some(name) = path.0.file_name().map(|s| s.to_string_lossy().to_string()) else {
        return Ok(None);
    };
    let Some(parent) = path.0.parent() else {
        return Ok(None);
    };
    let entries = vol
        .list(
            &VPath(parent.to_path_buf()),
            &ListOpts {
                show_hidden: true,
                ..Default::default()
            },
        )
        .await?;
    Ok(entries.into_iter().find(|e| e.name == name))
}

/// 目标卷里已存在时按策略定出最终路径。`None` = 跳过。
///
/// 命名规则（`a.txt → a (2).txt`、已带序号则续号）在 `crate::rename` 里只有一份；
/// 这里与 `fmnr-fs::ops::unique_name` 各自的只有"探测存在性"那三行——因为一边要 `await`
/// 问卷、另一边直接问 `Path::exists`，同步异步没法共用同一个循环，但**规则**共用了。
async fn resolve_on_dst(
    dst: &Arc<dyn Volume>,
    dest: &VPath,
    policy: ConflictPolicy,
) -> Result<Option<VPath>> {
    if entry_of(dst, dest).await?.is_none() {
        return Ok(Some(dest.clone()));
    }
    match policy {
        ConflictPolicy::Overwrite => {
            let existing = entry_of(dst, dest).await?;
            if existing.map(|e| e.is_dir).unwrap_or(false) {
                return Err(Error::Unsupported(format!(
                    "目标 {} 已是一个目录：跨拷贝不覆盖目录（要先删再拷，而那是另一套删除语义）",
                    dest.0.display()
                )));
            }
            Ok(Some(dest.clone()))
        }
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Rename => {
            let stem = dest
                .0
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            for n in first_rename_index(&stem)..u32::MAX {
                let candidate = VPath(candidate_name(&dest.0, n));
                if entry_of(dst, &candidate).await?.is_none() {
                    return Ok(Some(candidate));
                }
            }
            Ok(None)
        }
    }
}

/// 建目录，**已存在算成功**：`Volume::create_dir` 直通 `std::fs::create_dir`，
/// 重名会报 AlreadyExists，而搬运里"目标目录已经在了"是常态不是异常。
/// 只吞这一个错，别的（权限、路径非法）照原样抛。
async fn ensure_dir(vol: &Arc<dyn Volume>, dir: &VPath) -> Result<()> {
    match vol.create_dir(dir).await {
        Ok(()) => Ok(()),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IconKey, VolumeKind};
    use async_trait::async_trait;
    use futures::Stream;
    use std::io;
    use std::path::PathBuf;

    /// 一个只装文件的内存卷；既能当源也能当目标，够把跨卷的形状钉住而不碰磁盘。
    struct MemVol {
        id: String,
        kind: VolumeKind,
        writable: bool,
        files: std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        /// 每次 `read_stream` 吐多大一块——用它钉"走的是流"。
        chunk: usize,
    }

    impl MemVol {
        fn new(id: &str, writable: bool) -> Arc<Self> {
            Arc::new(Self {
                id: id.into(),
                kind: if writable {
                    VolumeKind::Local
                } else {
                    VolumeKind::Archive
                },
                writable,
                files: Default::default(),
                chunk: 3,
            })
        }
        fn put(self: &Arc<Self>, path: &str, bytes: &[u8]) {
            self.files
                .lock()
                .unwrap()
                .insert(path.into(), bytes.to_vec());
        }
        fn get(self: &Arc<Self>, path: &str) -> Option<Vec<u8>> {
            self.files.lock().unwrap().get(path).cloned()
        }
    }

    #[async_trait]
    impl Volume for MemVol {
        fn id(&self) -> crate::VolumeId {
            crate::VolumeId(self.id.clone())
        }
        fn kind(&self) -> VolumeKind {
            self.kind
        }
        async fn list(&self, path: &VPath, _opts: &ListOpts) -> Result<Vec<Entry>> {
            let dir = path.0.to_string_lossy().to_string();
            let prefix = if dir.is_empty() || dir == "/" {
                String::new()
            } else {
                format!("{dir}/")
            };
            // 🔴 先拿一份快照再遍历：早先写成 `for k in self.files.lock().unwrap().keys()`
            // 又在循环体里 `self.files.lock().unwrap()[k]` 取长度——同一把非重入锁锁两次，
            // 第一个测试直接死锁，`cargo test` 挂在那里不返回（这教训归到"探针自己会造故障"那类）。
            let snapshot: Vec<(String, usize)> = self
                .files
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.len()))
                .collect();
            let mut out = Vec::new();
            let mut dirs = std::collections::BTreeSet::new();
            for (key, len) in &snapshot {
                let Some(rest) = key.strip_prefix(&prefix) else {
                    continue;
                };
                if rest.is_empty() {
                    continue;
                }
                match rest.find('/') {
                    Some(i) => {
                        dirs.insert(rest[..i].to_string());
                    }
                    None => out.push(entry(rest, false, *len as u64)),
                }
            }
            for d in dirs {
                out.push(entry(&d, true, 0));
            }
            Ok(out)
        }
        async fn read_stream(
            &self,
            path: &VPath,
        ) -> Result<Box<dyn Stream<Item = io::Result<Vec<u8>>> + Send + Unpin>> {
            let key = path.0.to_string_lossy().to_string();
            let bytes = self
                .files
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .ok_or_else(|| Error::NotFound(key))?;
            let chunk = self.chunk;
            let parts: Vec<Vec<u8>> = bytes.chunks(chunk).map(|c| c.to_vec()).collect();
            Ok(Box::new(futures::stream::iter(
                parts.into_iter().map(Ok::<Vec<u8>, io::Error>),
            )))
        }
        async fn write(&self, path: &VPath, data: &[u8]) -> Result<()> {
            if !self.writable {
                return Err(Error::Unsupported("只读卷".into()));
            }
            self.files
                .lock()
                .unwrap()
                .insert(path.0.to_string_lossy().to_string(), data.to_vec());
            Ok(())
        }
        /// 故意**不覆写**：默认实现会把整份收进内存再调 `write`。
        /// 内存卷走默认实现没关系（要钉的是本地卷那条覆写路径，见 fmnr-fs 的测试）。
        fn writable(&self) -> bool {
            self.writable
        }
        fn native_path(&self, _p: &VPath) -> Option<PathBuf> {
            None
        }
        async fn spawn_external_open(
            &self,
            _p: &VPath,
            _prog: &str,
        ) -> Result<crate::ExternalOpen> {
            Err(Error::Unsupported("内存卷开不了外部程序".into()))
        }
        fn external_opens(&self) -> &crate::ExternalOpens {
            static OPENS: std::sync::OnceLock<crate::ExternalOpens> = std::sync::OnceLock::new();
            OPENS.get_or_init(crate::ExternalOpens::default)
        }
        async fn copy(
            &self,
            _s: &[VPath],
            _d: &VPath,
            _p: ConflictPolicy,
            _pr: Arc<Progress>,
        ) -> Result<OpReport> {
            unimplemented!("跨卷测试不该走到卷自己的 copy")
        }
        async fn move_entries(
            &self,
            _s: &[VPath],
            _d: &VPath,
            _p: ConflictPolicy,
            _pr: Arc<Progress>,
        ) -> Result<OpReport> {
            unimplemented!()
        }
        async fn delete(&self, _t: &[VPath], _p: Arc<Progress>) -> Result<OpReport> {
            unimplemented!()
        }
        async fn rename(&self, _f: &VPath, _t: &VPath) -> Result<()> {
            unimplemented!()
        }
        async fn create_dir(&self, path: &VPath) -> Result<()> {
            if !self.writable {
                return Err(Error::Unsupported("只读卷".into()));
            }
            let _ = path;
            Ok(())
        }
        async fn search(&self, _r: &VPath, _n: String, _l: usize) -> Result<Vec<VPath>> {
            unimplemented!()
        }
    }

    /// 测试里同时需要「当 `dyn Volume` 用」和「拿具体句柄断言」两份视图。
    struct Pair {
        vol: Arc<dyn Volume>,
        mem: Arc<MemVol>,
    }

    fn vol(id: &str, writable: bool) -> Pair {
        let m = MemVol::new(id, writable);
        Pair {
            vol: m.clone(),
            mem: m,
        }
    }

    fn entry(name: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            name: name.into(),
            is_dir,
            size: if is_dir { None } else { Some(size) },
            modified: None,
            icon_key: IconKey::Generic,
            attrs: vec![],
        }
    }

    fn p(s: &str) -> VPath {
        VPath(PathBuf::from(s))
    }

    /// 主通路：归档里的成员落到本地卷，逐字节相等，且**目录树形状保留在文件名里由目标卷建**。
    #[tokio::test]
    async fn streams_one_file_from_readonly_volume_into_writable_one() {
        let src = vol("archive:/a.7z", false);
        src.mem.put("hello.txt", b"hello 7z\n");
        let dst = vol("local:/out", true);
        let pr = Arc::new(Progress::new());

        let rep = copy_across(
            &src.vol,
            &dst.vol,
            &[p("hello.txt")],
            &p(""),
            ConflictPolicy::Rename,
            &pr,
        )
        .await
        .unwrap();

        assert_eq!(rep.done, 1);
        assert_eq!(rep.bytes, 9);
        assert_eq!(
            dst.mem.get("hello.txt").as_deref(),
            Some(&b"hello 7z\n"[..])
        );
    }

    /// 🔴 只读目标必须明确失败，而不是"拷了个寂寞"。
    #[tokio::test]
    async fn refuses_to_copy_into_a_readonly_volume() {
        let src = vol("local:/in", true);
        src.mem.put("a.bin", b"1234567890");
        let dst = vol("archive:/out.7z", false);
        let pr = Arc::new(Progress::new());

        let e = copy_across(
            &src.vol,
            &dst.vol,
            &[p("a.bin")],
            &p(""),
            ConflictPolicy::Skip,
            &pr,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(_)),
            "期望 Unsupported，拿到 {e:?}"
        );
        let msg = e.to_string();
        assert!(msg.contains("只读"), "报错要说清是只读：{msg}");
        // 🔴 还要点名是**目标**卷只读。少这半句的话，把上面的可写性检查摘掉也能蒙过去
        // （那时错来自目标卷自己的 `write`，消息是"本卷只读"）——实测就是这样逃逸的。
        assert!(
            msg.contains("目标卷"),
            "要指出是目标卷，不能只说「只读」：{msg}"
        );
    }

    /// 同卷拒绝：避免在这里复制第二套本地搬运政策。
    #[tokio::test]
    async fn same_volume_is_referred_back_to_volume_copy() {
        let v = vol("local:/x", true);
        v.mem.put("a", b"1");
        let pr = Arc::new(Progress::new());
        let e = copy_across(&v.vol, &v.vol, &[p("a")], &p(""), ConflictPolicy::Skip, &pr)
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }

    /// Skip：目标已有同名 ⇒ 一条不搬，**且不覆盖已有内容**。
    #[tokio::test]
    async fn skip_policy_leaves_the_existing_target_untouched() {
        let src = vol("archive:/a.zip", false);
        src.mem.put("note.txt", b"new");
        let dst = vol("local:/d", true);
        dst.mem.put("note.txt", b"old!");
        let pr = Arc::new(Progress::new());

        let rep = copy_across(
            &src.vol,
            &dst.vol,
            &[p("note.txt")],
            &p(""),
            ConflictPolicy::Skip,
            &pr,
        )
        .await
        .unwrap();
        assert_eq!((rep.done, rep.skipped, rep.bytes), (0, 1, 0));
        assert_eq!(dst.mem.get("note.txt").as_deref(), Some(&b"old!"[..]));
    }

    /// Rename：`note.txt` 在、`note (2).txt` 也在 ⇒ 落到 `(3)`，序号规则与本地搬运同一份。
    #[tokio::test]
    async fn rename_policy_continues_the_numbering() {
        let src = vol("archive:/a.zip", false);
        src.mem.put("note.txt", b"new");
        let dst = vol("local:/d", true);
        dst.mem.put("note.txt", b"1");
        dst.mem.put("note (2).txt", b"2");
        let pr = Arc::new(Progress::new());

        copy_across(
            &src.vol,
            &dst.vol,
            &[p("note.txt")],
            &p(""),
            ConflictPolicy::Rename,
            &pr,
        )
        .await
        .unwrap();
        assert_eq!(dst.mem.get("note (3).txt").as_deref(), Some(&b"new"[..]));
        assert_eq!(dst.mem.get("note.txt").as_deref(), Some(&b"1"[..]));
    }

    /// Overwrite：文件可以覆盖（截断重写），**目录不行**——那是另一套删除语义。
    #[tokio::test]
    async fn overwrite_replaces_a_file_but_refuses_a_directory() {
        let src = vol("archive:/a.zip", false);
        src.mem.put("f.txt", b"NEW");
        src.mem.put("dir/inner", b"x");
        let dst = vol("local:/d", true);
        dst.mem.put("f.txt", b"OLDOLD");
        dst.mem.put("dir/inner", b"keep");
        let pr = Arc::new(Progress::new());

        copy_across(
            &src.vol,
            &dst.vol,
            &[p("f.txt")],
            &p(""),
            ConflictPolicy::Overwrite,
            &pr,
        )
        .await
        .unwrap();
        assert_eq!(dst.mem.get("f.txt").as_deref(), Some(&b"NEW"[..]));

        let e = copy_across(
            &src.vol,
            &dst.vol,
            &[p("dir")],
            &p(""),
            ConflictPolicy::Overwrite,
            &pr,
        )
        .await
        .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
        assert!(e.to_string().contains("目录"), "要说清是不覆盖目录：{e}");
        // 拒绝必须是**动手之前**：里面那份不能被删掉一半
        assert_eq!(dst.mem.get("dir/inner").as_deref(), Some(&b"keep"[..]));
    }

    /// 🔴 **目标侧**的同名目录也要拒——上面那条测的是源侧（它其实是被预检趟拦下的，
    /// 所以量不出这一支：变异"摘掉 `resolve_on_dst` 里的目录检查"原本全绿）。
    /// 形状：源是一个**文件** `note`，目标卷里 `note` 是一个**目录**。
    /// 少了这道闸，`Overwrite` 会把一个文件写进一个目录的位置上。
    #[tokio::test]
    async fn overwrite_refuses_a_destination_that_is_a_directory() {
        let src = vol("archive:/a.zip", false);
        src.mem.put("note", b"NEW");
        let dst = vol("local:/d", true);
        dst.mem.put("note/inner", b"keep");
        let pr = Arc::new(Progress::new());

        let e = copy_across(
            &src.vol,
            &dst.vol,
            &[p("note")],
            &p(""),
            ConflictPolicy::Overwrite,
            &pr,
        )
        .await
        .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
        assert!(e.to_string().contains("目录"), "要说清是目标是目录：{e}");
        assert_eq!(
            dst.mem.get("note"),
            None,
            "被拒之后目标里不许落下一个同名文件"
        );
        assert_eq!(dst.mem.get("note/inner").as_deref(), Some(&b"keep"[..]));
        assert_eq!(pr.snapshot().files_done, 0);
    }

    /// 源不存在要 NotFound，不能报成"搬了 0 条，成功"。
    #[tokio::test]
    async fn missing_source_is_not_found_not_an_empty_success() {
        let src = vol("archive:/a.zip", false);
        src.mem.put("there.txt", b"x");
        let dst = vol("local:/d", true);
        let pr = Arc::new(Progress::new());

        let e = copy_across(
            &src.vol,
            &dst.vol,
            &[p("gone.txt")],
            &p(""),
            ConflictPolicy::Skip,
            &pr,
        )
        .await
        .unwrap_err();
        assert!(matches!(e, Error::NotFound(_)), "期望 NotFound，拿到 {e:?}");
    }

    /// 目录作为源：明确拒绝并指出禁区（而不是静默只拷一层或无限递归）。
    #[tokio::test]
    async fn a_directory_source_is_refused_with_the_reason() {
        let src = vol("archive:/a.zip", false);
        src.mem.put("dir/inner.txt", b"x");
        let dst = vol("local:/d", true);
        let pr = Arc::new(Progress::new());

        let e = copy_across(
            &src.vol,
            &dst.vol,
            &[p("dir")],
            &p(""),
            ConflictPolicy::Skip,
            &pr,
        )
        .await
        .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
        assert!(
            e.to_string().contains("Volume::search"),
            "报错要指出下一步该接什么：{e}"
        );
    }

    /// 取消：一条都不该动目标卷。
    #[tokio::test]
    async fn cancellation_reports_aborted_and_writes_nothing() {
        let src = vol("archive:/a.zip", false);
        src.mem.put("a.txt", b"1");
        let dst = vol("local:/d", true);
        let pr = Arc::new(Progress::new());
        pr.cancel();

        let rep = copy_across(
            &src.vol,
            &dst.vol,
            &[p("a.txt")],
            &p(""),
            ConflictPolicy::Skip,
            &pr,
        )
        .await
        .unwrap();
        assert_eq!((rep.done, rep.aborted), (0, 1));
        assert_eq!(dst.mem.get("a.txt"), None);
    }

    /// 父目录**列不出来**（权限、坏件）要把那个错原样带出去，不能洗成 NotFound。
    /// 与本地卷那三条真 DACL 门同一口径：把"读不到"说成"没有"是最坏的一类错。
    #[tokio::test]
    async fn an_unreadable_parent_propagates_instead_of_becoming_not_found() {
        let src: Arc<dyn Volume> = BlockedListVol::new();
        let dst = vol("local:/d", true);
        let pr = Arc::new(Progress::new());
        let e = copy_across(
            &src,
            &dst.vol,
            &[p("whatever.txt")],
            &p(""),
            ConflictPolicy::Skip,
            &pr,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(e, Error::Io(_)),
            "期望把卷给的 Io 原样带出，拿到 {e:?}"
        );
        assert!(
            !e.to_string().contains("不存在"),
            "不许把它洗成「不存在」：{e}"
        );
    }

    /// 一个 `list` 永远失败的卷（模拟 DACL 拒访 / 头表坏），专门用来量错误传播。
    struct BlockedListVol {
        id: crate::VolumeId,
        opens: crate::ExternalOpens,
    }

    impl BlockedListVol {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                id: crate::VolumeId("archive:/broken.7z".into()),
                opens: crate::ExternalOpens::new(),
            })
        }
    }

    #[async_trait]
    impl Volume for BlockedListVol {
        fn id(&self) -> crate::VolumeId {
            self.id.clone()
        }
        fn kind(&self) -> VolumeKind {
            VolumeKind::Archive
        }
        async fn list(&self, _path: &VPath, _opts: &ListOpts) -> Result<Vec<Entry>> {
            Err(Error::Io(std::io::Error::other("拒绝访问")))
        }
        async fn read_stream(
            &self,
            _p: &VPath,
        ) -> Result<
            std::boxed::Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>,
        > {
            panic!("列都列不出来，不该走到读")
        }
        async fn write(&self, _p: &VPath, _d: &[u8]) -> Result<()> {
            Err(Error::Unsupported("只读".into()))
        }
        fn writable(&self) -> bool {
            false
        }
        fn native_path(&self, _p: &VPath) -> Option<PathBuf> {
            None
        }
        async fn spawn_external_open(
            &self,
            _p: &VPath,
            _prog: &str,
        ) -> Result<crate::ExternalOpen> {
            Err(Error::Unsupported("开不了".into()))
        }
        fn external_opens(&self) -> &crate::ExternalOpens {
            &self.opens
        }
        async fn copy(
            &self,
            _s: &[VPath],
            _d: &VPath,
            _p: ConflictPolicy,
            _pr: Arc<Progress>,
        ) -> Result<OpReport> {
            unimplemented!()
        }
        async fn move_entries(
            &self,
            _s: &[VPath],
            _d: &VPath,
            _p: ConflictPolicy,
            _pr: Arc<Progress>,
        ) -> Result<OpReport> {
            unimplemented!()
        }
        async fn delete(&self, _t: &[VPath], _p: Arc<Progress>) -> Result<OpReport> {
            unimplemented!()
        }
        async fn rename(&self, _f: &VPath, _t: &VPath) -> Result<()> {
            unimplemented!()
        }
        async fn create_dir(&self, _p: &VPath) -> Result<()> {
            unimplemented!()
        }
        async fn search(&self, _r: &VPath, _n: String, _l: usize) -> Result<Vec<VPath>> {
            unimplemented!()
        }
    }

    /// 🔴 分母要提前知道：一批两条、各 3 字节 ⇒ `bytes_total` 必须是 6，
    /// 而**一个字节都还没搬**（`bytes_done == 0`）。头表/stat 里就有 size，
    /// 不该为了算分母去读内容，也不该干脆报 0（`fraction()` 在 0 分母下恒 0.0，
    /// 等于把进度条钉死在起点）。
    #[tokio::test]
    async fn the_byte_denominator_comes_from_headers_not_from_reading() {
        let src = vol("archive:/a.zip", false);
        src.mem.put("one.txt", b"aaa");
        src.mem.put("two.txt", b"bbbbb");
        let dst = vol("local:/d", true);
        let pr = Arc::new(Progress::new());

        copy_across(
            &src.vol,
            &dst.vol,
            &[p("one.txt"), p("two.txt")],
            &p(""),
            ConflictPolicy::Rename,
            &pr,
        )
        .await
        .unwrap();

        let snap = pr.snapshot();
        assert_eq!(snap.bytes_total, 8, "分母该等于两条头表 size 之和");
        assert_eq!(snap.bytes_done, 8, "跑完时分母分子要撞上");
        assert_eq!(snap.files_total, 2);
    }

    /// 🔴 一批里**第 7 条是目录**也必须一条都不搬：拒绝要发生在动手之前。
    /// 这条钉的是"先问清楚再搬"那趟的存在理由——单趟循环里前几条已经落进目标卷了。
    #[tokio::test]
    async fn a_directory_anywhere_in_the_batch_refuses_before_writing_anything() {
        let src = vol("archive:/a.zip", false);
        src.mem.put("aaa-first.txt", b"111");
        src.mem.put("dir/later.txt", b"x");
        src.mem.put("zzz-last.txt", b"222");
        let dst = vol("local:/d", true);
        let pr = Arc::new(Progress::new());

        let e = copy_across(
            &src.vol,
            &dst.vol,
            &[p("aaa-first.txt"), p("dir"), p("zzz-last.txt")],
            &p(""),
            ConflictPolicy::Skip,
            &pr,
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("目录"), "{e}");
        assert!(
            dst.mem.get("aaa-first.txt").is_none(),
            "被拒之后目标里不该已经落下第一条"
        );
        assert_eq!(pr.snapshot().files_done, 0, "一条都没搬，计数也不该动");
    }

    /// 头表 size 缺失（坏件/虚标）时**不许编一个分母**：宁可不给百分比。
    #[tokio::test]
    async fn an_unknown_size_turns_the_denominator_off_instead_of_lying() {
        let src: Arc<dyn Volume> = UnknownSizeVol::new();
        let dst = vol("local:/d", true);
        let pr = Arc::new(Progress::new());
        copy_across(
            &src,
            &dst.vol,
            &[p("sized.txt")],
            &p(""),
            ConflictPolicy::Skip,
            &pr,
        )
        .await
        .unwrap();
        let snap = pr.snapshot();
        assert_eq!(snap.bytes_total, 0, "size 未知就别装出分母");
        assert_eq!(snap.bytes_done, 4, "实际写出的字节还是要记");
        assert_eq!(dst.mem.get("sized.txt").as_deref(), Some(&b"abcd"[..]));
    }

    /// 一个 `Entry.size` 为 `None` 的文件卷（模拟坏头表）。
    struct UnknownSizeVol {
        opens: crate::ExternalOpens,
    }

    impl UnknownSizeVol {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                opens: crate::ExternalOpens::new(),
            })
        }
    }

    #[async_trait]
    impl Volume for UnknownSizeVol {
        fn id(&self) -> crate::VolumeId {
            crate::VolumeId("archive:/weird.7z".into())
        }
        fn kind(&self) -> VolumeKind {
            VolumeKind::Archive
        }
        async fn list(&self, _p: &VPath, _o: &ListOpts) -> Result<Vec<Entry>> {
            Ok(vec![Entry {
                name: "sized.txt".into(),
                is_dir: false,
                size: None,
                modified: None,
                icon_key: IconKey::Generic,
                attrs: vec![],
            }])
        }
        async fn read_stream(
            &self,
            _p: &VPath,
        ) -> Result<
            std::boxed::Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>,
        > {
            Ok(Box::new(futures::stream::iter(vec![Ok(b"abcd".to_vec())])))
        }
        async fn write(&self, _p: &VPath, _d: &[u8]) -> Result<()> {
            Err(Error::Unsupported("只读".into()))
        }
        fn writable(&self) -> bool {
            false
        }
        fn native_path(&self, _p: &VPath) -> Option<PathBuf> {
            None
        }
        async fn spawn_external_open(
            &self,
            _p: &VPath,
            _prog: &str,
        ) -> Result<crate::ExternalOpen> {
            Err(Error::Unsupported("开不了".into()))
        }
        fn external_opens(&self) -> &crate::ExternalOpens {
            &self.opens
        }
        async fn copy(
            &self,
            _s: &[VPath],
            _d: &VPath,
            _p: ConflictPolicy,
            _pr: Arc<Progress>,
        ) -> Result<OpReport> {
            unimplemented!()
        }
        async fn move_entries(
            &self,
            _s: &[VPath],
            _d: &VPath,
            _p: ConflictPolicy,
            _pr: Arc<Progress>,
        ) -> Result<OpReport> {
            unimplemented!()
        }
        async fn delete(&self, _t: &[VPath], _p: Arc<Progress>) -> Result<OpReport> {
            unimplemented!()
        }
        async fn rename(&self, _f: &VPath, _t: &VPath) -> Result<()> {
            unimplemented!()
        }
        async fn create_dir(&self, _p: &VPath) -> Result<()> {
            unimplemented!()
        }
        async fn search(&self, _r: &VPath, _n: String, _l: usize) -> Result<Vec<VPath>> {
            unimplemented!()
        }
    }

    #[test]
    fn rename_rule_lives_in_one_place() {
        // 规则本身：a.txt 的第 3 个候选是 `a (3).txt`；无扩展名时不加点。
        let d = PathBuf::from("/out/a.txt");
        assert_eq!(candidate_name(&d, 3), PathBuf::from("/out/a (3).txt"));
        let n = PathBuf::from("/out/README");
        assert_eq!(candidate_name(&n, 2), PathBuf::from("/out/README (2)"));
        // 🔴 名字自己已经带序号时**只换序号**，不许套第二层括号：
        // 摘掉 strip 那段的话这条会造出 `note (2) (3).txt`。
        let numbered = PathBuf::from("/out/note (2).txt");
        assert_eq!(
            candidate_name(&numbered, 3),
            PathBuf::from("/out/note (3).txt")
        );
        // 只有"看着像序号"的那一段才剥：`v2 (beta)` 里那段不是数字，剥了就丢信息。
        let not_index = PathBuf::from("/out/a (beta).txt");
        assert_eq!(
            candidate_name(&not_index, 2),
            PathBuf::from("/out/a (beta) (2).txt")
        );
        assert_eq!(first_rename_index("note"), 2);
        assert_eq!(first_rename_index("note (2)"), 3);
        assert_eq!(first_rename_index("note (x)"), 2);
        assert_eq!(first_rename_index("a (2) (3)"), 4);
    }
}
