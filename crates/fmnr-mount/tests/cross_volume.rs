//! 跨卷搬运的组合门：真归档卷（只读）+ 真本地卷（可写）走同一条通路。
//!
//! 单元测试（`fmnr-core::transfer`、`fmnr-fs::ops`）已经各自钉住了形状与循环，
//! 这一格钉的是**装在一起还能用**：源用真 7-Zip 26.00 写出的件（与
//! `crates/fmnr-archive/tests/sevenzip.rs` 同一份字节，见下方路径注释），
//! 目标用真盘上的临时目录。
//!
//! 顺带钉一条**实测到的旧缺陷**：归档卷 `search` 吐出来的命中形如
//! `archive:<件>!<卷内路径>`，而 `key_of` 原先只认 `/`，于是
//! "搜到 → 拿这条去读"直接废（实测 `os error 123`）。搜索命中的用途就是被回读。

use fmnr_core::{
    transfer::copy_across, volreg::VolumeRegistry, ConflictPolicy, ListOpts, Progress, VPath,
    Volume,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 复用 fmnr-archive 那份**真 7-Zip 字节**（223 B，sha 登记在
/// `crates/fmnr-archive/tests/sevenzip.rs` 文件头，那边有长度+签名的门守着）。
/// 刻意不在这里再拷一份：两份同样的字节会各自漂移，而漂移的那一份没人看得见。
fn plain7z() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fmnr-archive/tests/fixtures/plain.7z")
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fmnr-xvol-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

/// 🔴 主通路：把 7z 里的一个成员取到本地目录。
/// 修之前这条在任何入口都做不到——路由选中归档卷后，目标目录也在那个卷里解析，
/// 报的是「归档卷是只读的」。
#[test]
fn extracts_a_member_from_a_real_7z_into_a_local_directory() {
    let src: Arc<dyn Volume> = Arc::new(fmnr_archive::ArchiveVolume::open(plain7z()).unwrap());
    let d = tmpdir("extract");
    let dst: Arc<dyn Volume> = Arc::new(fmnr_fs::LocalVolume::new(d.clone()));
    let pr = Arc::new(Progress::new());

    let rep = rt()
        .block_on(copy_across(
            &src,
            &dst,
            &[VPath(PathBuf::from("archive:/hello.txt"))],
            &VPath(d.clone()),
            ConflictPolicy::Rename,
            &pr,
        ))
        .expect("取包内成员到本地该成功");

    assert_eq!(rep.done, 1);
    assert_eq!(rep.bytes, 9, "hello.txt 在头表里就是 9 字节");
    assert_eq!(
        std::fs::read(d.join("hello.txt")).unwrap(),
        b"hello 7z\n",
        "内容必须逐字节等于包里的成员"
    );
    std::fs::remove_dir_all(&d).ok();
}

/// 嵌套成员：源侧那条路径的**卷内 key** 是 `dir/deep.bin`，落到本地时
/// 文件名取 `deep.bin`（跨卷拷贝刻意只取末段名字；保留整棵子树是"目录搬运"那件事，
/// 它有自己的禁区与前置条件）。
#[test]
fn a_nested_member_lands_by_its_own_name() {
    let src: Arc<dyn Volume> = Arc::new(fmnr_archive::ArchiveVolume::open(plain7z()).unwrap());
    let d = tmpdir("nested");
    let dst: Arc<dyn Volume> = Arc::new(fmnr_fs::LocalVolume::new(d.clone()));
    let pr = Arc::new(Progress::new());

    rt().block_on(copy_across(
        &src,
        &dst,
        &[VPath(PathBuf::from("archive:/dir/deep.bin"))],
        &VPath(d.clone()),
        ConflictPolicy::Rename,
        &pr,
    ))
    .unwrap();

    assert_eq!(
        std::fs::read(d.join("deep.bin")).unwrap(),
        b"ABCDEFGHIJ",
        "嵌套成员要按字节取出来，且落在目标目录根上（不建子树）"
    );
    assert!(
        !d.join("dir").exists(),
        "单文件跨卷拷贝不该顺手建出源侧的父目录"
    );
    std::fs::remove_dir_all(&d).ok();
}

/// 归档卷 `search` 的命中必须能**原样回读**（这是 `!/` 那条形状的门）。
#[test]
fn search_hits_can_be_read_back() {
    let vol = fmnr_archive::ArchiveVolume::open(plain7z()).unwrap();
    let hits = rt()
        .block_on(vol.search(&VPath(PathBuf::from("archive:/")), "deep".into(), 50))
        .expect("卷根搜索本身要成功——报 Err 就不算'搜到能读回'");
    assert!(!hits.is_empty(), "搜 deep 该有命中（没命中这条门就空转了）");
    // 卷根的搜索在归档卷上按 key 走，命中形如 `archive:<件>!/dir/deep.bin`；
    // 这里把它原样喂回去读——认不下 `!` 的老实现会在这里变成 NotFound/语法错。
    for h in hits {
        let text = rt().block_on(read_all(&vol, &h));
        assert!(
            !text.is_empty(),
            "搜到的这条回读成了空：{}（搜索命中的用途就是被回读）",
            h.0.display()
        );
    }
}

async fn read_all(vol: &fmnr_archive::ArchiveVolume, p: &VPath) -> Vec<u8> {
    use futures::StreamExt;
    let mut s = vol.read_stream(p).await.expect("搜索命中要能回读");
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

/// 目标卷选上了才叫跨卷：注册表里那个本地卷必须能被一条**裸绝对路径**认出来，
/// 而源侧的归档卷不会被它抢走。
#[test]
fn dest_routing_picks_the_local_volume_for_a_plain_absolute_path() {
    let d = tmpdir("route");
    let reg = VolumeRegistry::new();
    let archive: Arc<dyn Volume> = Arc::new(fmnr_archive::ArchiveVolume::open(plain7z()).unwrap());
    let local: Arc<dyn Volume> = Arc::new(fmnr_fs::LocalVolume::new(d.clone()));
    reg.add(archive);
    reg.add(local);

    let picked = reg
        .route_for_dest(&d.to_string_lossy())
        .expect("裸绝对路径要能认到本地卷");
    assert!(
        picked.id().0.starts_with("local:"),
        "目标目录该认给本地卷，拿到 {}",
        picked.id().0
    );
    std::fs::remove_dir_all(&d).ok();
}

/// 列目录时隐藏项的口径与搬运无关，但**默认不显示隐藏**这条得记住：
/// `copy_across` 走的是 `entry_of`（`show_hidden: true`），所以 `.hide` 这种
/// 点开头的成员也能被显式取出来——不会因为"界面上没显示"就取不到。
#[test]
fn a_hidden_member_is_still_reachable_by_name() {
    let src: Arc<dyn Volume> = Arc::new(fmnr_archive::ArchiveVolume::open(plain7z()).unwrap());
    let d = tmpdir("hidden");
    let dst: Arc<dyn Volume> = Arc::new(fmnr_fs::LocalVolume::new(d.clone()));
    let pr = Arc::new(Progress::new());

    rt().block_on(copy_across(
        &src,
        &dst,
        &[VPath(PathBuf::from("archive:/.hide"))],
        &VPath(d.clone()),
        ConflictPolicy::Rename,
        &pr,
    ))
    .unwrap();
    assert_eq!(std::fs::read(d.join(".hide")).unwrap(), b"dot\n");
    std::fs::remove_dir_all(&d).ok();
}

/// 归档卷默认列出时不含 `.hide`（与 `show_hidden` 的口径一致）——
/// 上面那条用显式名字取，不受这条影响。留在这里是为了说明两者不是一回事。
#[test]
fn listing_hides_it_by_default() {
    let vol = fmnr_archive::ArchiveVolume::open(plain7z()).unwrap();
    let names: Vec<String> = rt()
        .block_on(vol.list(&VPath(PathBuf::from("archive:/")), &ListOpts::default()))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(
        !names.contains(&".hide".to_string()),
        "默认不该列出：{names:?}"
    );
    assert!(names.contains(&"hello.txt".to_string()));
}
