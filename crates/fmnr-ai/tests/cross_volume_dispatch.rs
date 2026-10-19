//! 分派层的跨卷门：`dispatch_on_registry` 会不会真的走第二个卷。
//!
//! `fmnr-mount/tests/cross_volume.rs` 量的是内核那半（真归档字节 → 本地盘），
//! 这一格量的是**三条入口共用的那张表**：选源卷 + 认目标卷 + 只读目标要拒 +
//! 不带前缀的写法保持老行为 + `move` **不许**被特判（跨卷 move 本来就是文件系统级移动，
//! 见 `docs/NEXT-BATCH.md` §23 那条自我推翻）。用两个 `LocalVolume` 当一对卷——它们的 id 就是
//! `local:<根>`，正好把"裸绝对路径按根最长匹配"这条规则也一并钉住。

use fmnr_core::{volreg::VolumeRegistry, VPath, Volume};
use fmnr_fs::LocalVolume;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 本测试二进制的公共临时根：`<temp>/fmnr-xdiv-<pid>`，一次建好。
/// 各测试只在自己的子目录里活动 ⇒ 路径短、且**永远不可能撞到别人的目录**。
fn root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("fmnr-xdiv-{}", std::process::id()));
        std::fs::create_dir_all(&d)
            .unwrap_or_else(|e| panic!("建公共临时根 {} 失败：{e}", d.display()));
        d
    })
    .clone()
}

/// 每个测试一个**独占**子目录。
/// 🔴 三条纪律，都是一轮轮实测换来的：
/// ① 名字要带每次调用都不同的序号——只带 pid 的话同一趟里几个测试拿到同一个目录，
///    谁先收尾就把对方的文件删了（实测红过一条"源不见了"，而代码没错）；
/// ② **不许在建之前先 `remove_dir_all`**——Windows 上"已标记删除"的目录还会在原地挂一会儿，
///    紧接着的 `create_dir_all` 就可能拿到 `ERROR_PATH_NOT_FOUND(3)`（CI 的 windows job
///    实测判红过两条，本地 TMPDIR 下不复现）；序号本来就唯一，压根没有同名旧目录要清；
/// ③ 建失败要把**路径**打进 panic 消息——判据红在文件系统上时，没有路径就只能靠猜。
fn tmpdir(tag: &str) -> PathBuf {
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = root().join(format!("{tag}-{seq}"));
    std::fs::create_dir_all(&d).unwrap_or_else(|e| panic!("建临时目录 {} 失败：{e}", d.display()));
    d
}

/// 读落地文件；**读不到就把整棵临时根的文件清单打出来**。
/// 加这条的理由：`copy_lands_on_the_other_volume` 在 ubuntu job 判红过
/// （回执 `done:1`、`bytes:6`，但目标目录里没有那个文件），本机三遍不复现。
/// 只报 `ENOENT` 定位不了那 6 个字节落在哪儿，所以把"落在哪儿"变成失败消息的一部分。
fn read_landed(p: &Path) -> Vec<u8> {
    match std::fs::read(p) {
        Ok(b) => b,
        Err(e) => panic!(
            "读 {} 失败：{e}\n临时根 {} 里的全部文件：{:?}",
            p.display(),
            root().display(),
            tree_files(&root())
        ),
    }
}

/// 目录树里所有**文件**的相对路径（排序，好比对）。
fn tree_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(dir) {
                out.push(rel.to_string_lossy().to_string());
            }
        }
    }
    out.sort();
    out
}

/// 一对本地卷。**A 的根刻意比 B 深一层**：`route_for` 是"最长 id 前缀赢"，
/// 而 `path_samples` 把 `sources` 和 `dest_dir` 混在一起看，两个 id 一样长时
/// 谁赢取决于谁先被扫到——那样测的就不是"源卷 + 另一个目标卷"，而是碰运气。
/// 深一层之后源侧那条路径必然把 A 选出来，跨卷这件事才是被钉住的。
fn two_volumes() -> (VolumeRegistry, PathBuf, PathBuf) {
    let a = tmpdir("a").join("deeper");
    let b = tmpdir("b");
    std::fs::create_dir_all(&a).unwrap();
    let reg = VolumeRegistry::new();
    let va: Arc<dyn Volume> = Arc::new(LocalVolume::new(a.clone()));
    let vb: Arc<dyn Volume> = Arc::new(LocalVolume::new(b.clone()));
    reg.add(va);
    reg.add(vb);
    (reg, a, b)
}

async fn call(
    reg: &VolumeRegistry,
    tool: &str,
    args: Value,
) -> Result<Value, fmnr_ai::protocol::RpcError> {
    fmnr_ai::tools::dispatch_on_registry(reg, tool, args).await
}

/// 🔴 主通路：源带 A 卷前缀、目标写 B 卷的绝对路径 ⇒ 文件必须落在 **B**，
/// 而不是被"目标也在某个卷里解析"那条老规则搬回 A。
#[tokio::test]
async fn copy_lands_on_the_other_volume() {
    let (reg, a, b) = two_volumes();
    std::fs::write(a.join("note.txt"), b"from-A").unwrap();

    let out = call(
        &reg,
        "copy",
        json!({
            "sources": [format!("local:{}", a.join("note.txt").display())],
            "dest_dir": format!("local:{}", b.display()),
            "conflict": "rename"
        }),
    )
    .await
    .expect("跨卷 copy 该成功");
    assert_eq!(out["done"], 1, "回执要说搬了几条：{out}");
    assert_eq!(out["bytes"], 6);
    assert_eq!(read_landed(&b.join("note.txt")), b"from-A");
    assert!(
        !a.join("note.txt").with_file_name("note (2).txt").exists(),
        "目标在 B，就不该在 A 里长出重名副本"
    );
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
}

/// 🔴 两个卷的根**嵌套**（源在浅根、目标在深根）时也必须真的跨卷。
/// 这条测的是 `route_source`：老规则把 `dest_dir` 也算进选卷样本，于是深根那个
/// 被选成**源卷**（`route_for` 取最长 id 前缀）⇒ 跨卷那一步压根走不到。
/// 🔴 但这条门**单独钉不住那个变异**（复打时它照样绿）：`LocalVolume` 收根外的绝对路径，
/// 退回单卷 copy 后盘上还是对的。分辨力在 `volreg::tests::the_source_volume_ignores_the_destination`。
#[tokio::test]
async fn nested_roots_copy_from_the_shallower_source_volume() {
    let shallow = tmpdir("shallow");
    let deep = shallow.join("deep");
    std::fs::create_dir_all(&deep).unwrap();
    let reg = VolumeRegistry::new();
    let vs: Arc<dyn Volume> = Arc::new(LocalVolume::new(shallow.clone()));
    let vd: Arc<dyn Volume> = Arc::new(LocalVolume::new(deep.clone()));
    reg.add(vs);
    reg.add(vd);
    std::fs::write(shallow.join("x.txt"), b"SHALLOW").unwrap();

    let out = call(
        &reg,
        "copy",
        json!({
            "sources": [format!("local:{}", shallow.join("x.txt").display())],
            "dest_dir": format!("local:{}", deep.display()),
            "conflict": "rename"
        }),
    )
    .await
    .unwrap_or_else(|e| panic!("浅根→深根该跨卷成功，拿到 {e:?}"));

    assert_eq!(out["done"], 1, "回执要说搬了几条：{out}");
    assert_eq!(read_landed(&deep.join("x.txt")), b"SHALLOW");
    std::fs::remove_dir_all(&shallow).ok();
}

/// **不带卷前缀的写法必须保持老行为**（目标在源卷里解析）。
/// 这条是对"改动不许悄悄改变既有命令的含义"的反证：
/// 一旦 `cross_target` 改成无条件认裸绝对路径，这里就会搬到 B 去并判红。
#[tokio::test]
async fn a_bare_dest_still_resolves_inside_the_source_volume() {
    let (reg, a, b) = two_volumes();
    std::fs::write(a.join("x.txt"), b"1").unwrap();

    call(
        &reg,
        "copy",
        json!({"sources": ["x.txt"], "dest_dir": "sub", "conflict": "skip"}),
    )
    .await
    .unwrap();
    assert!(a.join("sub").join("x.txt").exists(), "该落在 A 卷的 sub 下");
    assert!(!b.join("sub").exists(), "不该跑到 B 去");
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
}

/// 🔴 目标是**裸绝对路径**（不带 `local:` 前缀）时也要认到那个本地卷。
/// 这条专门钉 `route_for_dest` 的第二档：只认 id 前缀的那档由上面几条覆盖，
/// 而真用户写 `fmnr cp archive:X.zip/a.txt --to D:\out` 给的就是裸路径（实测 CLI 就是这么通的）。
#[tokio::test]
async fn a_bare_absolute_dest_is_recognised_without_a_volume_prefix() {
    let (reg, a, b) = two_volumes();
    std::fs::write(a.join("bare.txt"), b"bare-dest").unwrap();

    call(
        &reg,
        "copy",
        json!({
            "sources": [format!("local:{}", a.join("bare.txt").display())],
            // 刻意不带 `local:` —— 全靠"按目录成分比谁含它 + 最长匹配"
            "dest_dir": b.display().to_string(),
            "conflict": "rename"
        }),
    )
    .await
    .expect("裸绝对路径的目标该被认成另一个本地卷");

    assert_eq!(read_landed(&b.join("bare.txt")), b"bare-dest");
    assert!(a.join("bare.txt").exists(), "copy 不动源");
    std::fs::remove_dir_all(&a).ok();
    let parent = b.parent().unwrap().to_path_buf();
    std::fs::remove_dir_all(&parent).ok();
}

/// 🔴 真用户那条：**源是只读卷、目标是裸绝对路径**（不带 `local:` 前缀）。
/// 这条专门钉分派层要认裸路径——只认 id 前缀的话，源卷选中只读卷之后目标也在它里面解析，
/// 就又退回修前那个「归档卷是只读的」死路（实测的原始缺陷形状）。
#[tokio::test]
async fn extracting_from_a_readonly_source_to_a_bare_absolute_dest() {
    let ro_root = tmpdir("ex-src");
    let dst_root = tmpdir("ex-dst");
    let reg = VolumeRegistry::new();
    reg.add(Arc::new(ReadOnlyVolume::new(ro_root.clone())) as Arc<dyn Volume>);
    reg.add(Arc::new(LocalVolume::new(dst_root.clone())) as Arc<dyn Volume>);
    std::fs::write(ro_root.join("member.bin"), b"from-readonly").unwrap();

    let out = call(
        &reg,
        "copy",
        json!({
            "sources": [format!("ro:{}", ro_root.join("member.bin").display())],
            "dest_dir": dst_root.display().to_string(),
            "conflict": "rename"
        }),
    )
    .await
    .expect("从只读卷取到本地裸路径该成功（这正是本轮修的那条死路）");

    assert_eq!(out["done"], 1, "{out}");
    assert_eq!(read_landed(&dst_root.join("member.bin")), b"from-readonly");
    std::fs::remove_dir_all(&ro_root).ok();
    std::fs::remove_dir_all(&dst_root).ok();
}

/// 🔴 目标是**源卷自己**时不许走跨卷那条（否则用户会遇到"同卷搬运请走 Volume::copy"
/// 这种内部话）。这条门专治"任何目标都算跨卷"的写法：少了 id 比较那一步，
/// 普通的同卷 cp 会被派去 `copy_across` 然后当场报错。
#[tokio::test]
async fn a_dest_inside_the_source_volume_stays_same_volume() {
    let (reg, a, b) = two_volumes();
    std::fs::write(a.join("same.txt"), b"stays").unwrap();
    let sub = a.join("out");

    call(
        &reg,
        "copy",
        json!({
            "sources": [format!("local:{}", a.join("same.txt").display())],
            // 裸绝对路径，但它就在**源卷自己**的根下面
            "dest_dir": sub.display().to_string(),
            "conflict": "skip"
        }),
    )
    .await
    .unwrap_or_else(|e| panic!("同卷目标不该被当成跨卷：{}", e.message));

    assert_eq!(read_landed(&sub.join("same.txt")), b"stays");
    std::fs::remove_dir_all(&b).ok();
    std::fs::remove_dir_all(a.parent().unwrap()).ok();
}

/// 只读目标：错要说在"**目标**卷是只读的"，而不是让人以为是源卷读不了。
#[tokio::test]
async fn a_readonly_target_says_so_about_the_target() {
    let src_root = tmpdir("ro-src");
    let ro: Arc<dyn Volume> = Arc::new(ReadOnlyVolume::new(src_root.clone()));
    let reg = VolumeRegistry::new();
    reg.add(Arc::new(LocalVolume::new(src_root.clone())) as Arc<dyn Volume>);
    reg.add(ro);
    std::fs::write(src_root.join("f.bin"), b"payload").unwrap();

    let e = call(
        &reg,
        "copy",
        json!({
            "sources": [format!("local:{}", src_root.join("f.bin").display())],
            "dest_dir": format!("ro:{}", src_root.display()),
            "conflict": "skip"
        }),
    )
    .await
    .unwrap_err();
    assert!(
        e.message.contains("目标卷") && e.message.contains("只读"),
        "报错要点名是目标卷只读，拿到：{}",
        e.message
    );
    std::fs::remove_dir_all(&src_root).ok();
}

/// `move` **不做跨卷特判**——这条测试钉的是"我没偷偷加一层拒绝"。
///
/// 原先这里写的是一条"跨卷 move 必须被拒、并说明源侧删除语义两边不同"的门。
/// **实测把它推翻了**：两个本地卷之间的 `move` 本来就成功（`LocalVolume` 认绝对路径，
/// 源被解析到另一个盘的根之外，它就是一次文件系统级移动），
/// 在分派层加一层拒绝只会把本来能用的一条弄坏。
/// 所以 `cross_target` 只对 `copy` 返回第二个卷；源真是只读卷时，
/// 由该卷自己的 `move_entries` 拒（消息是"归档卷是只读的"）。
#[tokio::test]
async fn move_is_not_special_cased_across_two_local_volumes() {
    let (reg, a, b) = two_volumes();
    std::fs::write(a.join("m.txt"), b"mv").unwrap();

    call(
        &reg,
        "move",
        json!({
            "sources": [format!("local:{}", a.join("m.txt").display())],
            "dest_dir": format!("local:{}", b.display()),
            "conflict": "rename"
        }),
    )
    .await
    .expect("两个本地卷之间的 move 不该被分派层拦下来");

    assert!(b.join("m.txt").exists(), "文件要落到目标卷");
    assert!(!a.join("m.txt").exists(), "移动嘛，源侧要少掉");
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
}

/// 一个"能读、能列、但写不了"的卷——用来当只读目标，
/// 形状与归档卷一致（`writable() == false`），但不依赖磁盘上的件。
struct ReadOnlyVolume {
    root: PathBuf,
    opens: fmnr_core::ExternalOpens,
}

impl ReadOnlyVolume {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            opens: fmnr_core::ExternalOpens::new(),
        }
    }

    /// 剥掉自己卷 id 的 `ro:` 前缀。探针卷也得认自己的寻址写法，
    /// 否则"列父目录"会拿整串 `ro:<那个根>/…` 去 `read_dir`。
    /// 🔴 剥完**原样当路径**用，不许再 `replace('\\',"/")` + `trim_start_matches('/')`：
    /// 那样在 unix 上会把前导 `/` 削掉 ⇒ 得到一个相对路径，`root.join(...)` 拼出一个
    /// 根本不存在的目录（ubuntu job 实测 `io: No such file or directory`；本地 Windows
    /// 因为路径以盘符 `C:` 开头、没有前导斜杠可削，所以看不出来）。
    /// 卷根（空串或只剩前缀）回落到自己的 root。
    fn inner(&self, path: &VPath) -> PathBuf {
        match path.0.to_string_lossy().strip_prefix("ro:") {
            Some(rest) if !rest.is_empty() => PathBuf::from(rest),
            _ => self.root.clone(),
        }
    }
}

#[async_trait::async_trait]
impl Volume for ReadOnlyVolume {
    fn id(&self) -> fmnr_core::VolumeId {
        fmnr_core::VolumeId(format!("ro:{}", self.root.display()))
    }
    fn kind(&self) -> fmnr_core::VolumeKind {
        fmnr_core::VolumeKind::Archive
    }
    async fn list(
        &self,
        path: &VPath,
        _opts: &fmnr_core::ListOpts,
    ) -> fmnr_core::Result<Vec<fmnr_core::Entry>> {
        let dir = self.inner(path);
        let mut out = Vec::new();
        for e in std::fs::read_dir(&dir).map_err(fmnr_core::Error::Io)? {
            let e = e.map_err(fmnr_core::Error::Io)?;
            let is_dir = e.file_type().map_err(fmnr_core::Error::Io)?.is_dir();
            out.push(fmnr_core::Entry {
                name: e.file_name().to_string_lossy().to_string(),
                is_dir,
                size: if is_dir {
                    None
                } else {
                    Some(e.metadata().map_err(fmnr_core::Error::Io)?.len())
                },
                modified: None,
                icon_key: fmnr_core::IconKey::Generic,
                attrs: vec![],
            });
        }
        Ok(out)
    }
    async fn read_stream(
        &self,
        path: &VPath,
    ) -> fmnr_core::Result<
        std::boxed::Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>,
    > {
        let bytes = std::fs::read(self.inner(path)).map_err(fmnr_core::Error::Io)?;
        Ok(Box::new(futures::stream::iter(vec![Ok(bytes)])))
    }
    async fn write(&self, _path: &VPath, _data: &[u8]) -> fmnr_core::Result<()> {
        Err(fmnr_core::Error::Unsupported("本卷只读".into()))
    }
    fn writable(&self) -> bool {
        false
    }
    fn native_path(&self, path: &VPath) -> Option<PathBuf> {
        Some(self.inner(path))
    }
    async fn spawn_external_open(
        &self,
        _p: &VPath,
        _prog: &str,
    ) -> fmnr_core::Result<fmnr_core::ExternalOpen> {
        Err(fmnr_core::Error::Unsupported("开不了".into()))
    }
    fn external_opens(&self) -> &fmnr_core::ExternalOpens {
        &self.opens
    }
    async fn copy(
        &self,
        _s: &[VPath],
        _d: &VPath,
        _p: fmnr_core::ConflictPolicy,
        _pr: Arc<fmnr_core::Progress>,
    ) -> fmnr_core::Result<fmnr_core::OpReport> {
        Err(fmnr_core::Error::Unsupported("本卷只读".into()))
    }
    async fn move_entries(
        &self,
        _s: &[VPath],
        _d: &VPath,
        _p: fmnr_core::ConflictPolicy,
        _pr: Arc<fmnr_core::Progress>,
    ) -> fmnr_core::Result<fmnr_core::OpReport> {
        Err(fmnr_core::Error::Unsupported("本卷只读".into()))
    }
    async fn delete(
        &self,
        _t: &[VPath],
        _p: Arc<fmnr_core::Progress>,
    ) -> fmnr_core::Result<fmnr_core::OpReport> {
        Err(fmnr_core::Error::Unsupported("本卷只读".into()))
    }
    async fn rename(&self, _f: &VPath, _t: &VPath) -> fmnr_core::Result<()> {
        Err(fmnr_core::Error::Unsupported("本卷只读".into()))
    }
    async fn create_dir(&self, _p: &VPath) -> fmnr_core::Result<()> {
        Err(fmnr_core::Error::Unsupported("本卷只读".into()))
    }
    async fn search(&self, _r: &VPath, _n: String, _l: usize) -> fmnr_core::Result<Vec<VPath>> {
        Ok(Vec::new())
    }
}
