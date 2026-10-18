//! 归档卷验收：用**真归档文件**（zip/tar/tar.gz）跑通「挂载 → 列目录 → 读成员」。
//!
//! 夹具在测试里现场生成，不入库二进制；这样格式假设一旦不成立，
//! 测试会立刻炸而不是悄悄通过。

use fmnr_archive::{split_path, ArchiveVolume};
use fmnr_core::{Error, ListOpts, Progress, SortOrder, VPath, Volume, VolumeKind};
use futures::StreamExt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn tmpdir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("fmnr-arc-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 造一个 zip：`readme.md`、`src/main.rs`、`src/util/mod.rs`、`.hidden`
fn make_zip(path: &Path) {
    // zip 9 的 FileOptions 多了一个扩展类型参数，不标注就推不出来。
    let opts: zip::write::FileOptions<'_, '_, zip::write::ExtendedFileOptions> =
        zip::write::FileOptions::default();
    let opts = opts.compression_method(zip::CompressionMethod::Deflated);
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, data) in [
        ("readme.md", b"# Title\nbody\n".as_slice()),
        ("src/main.rs", b"fn main() {}\n".as_slice()),
        ("src/util/mod.rs", b"// util\n".as_slice()),
        (".hidden", b"secret\n".as_slice()),
    ] {
        w.start_file(name, opts.clone()).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap();
}

fn make_tar(path: &Path, gz: bool) {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in [
        ("docs/a.txt", b"AAA".as_slice()),
        ("docs/b/c.txt", b"CCC".as_slice()),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_path(name).unwrap();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, name, data).unwrap();
    }
    let raw = b.into_inner().unwrap();
    let out: Vec<u8> = if gz {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        enc.finish().unwrap()
    } else {
        raw
    };
    std::fs::write(path, out).unwrap();
}

fn vp(s: &str) -> VPath {
    VPath(PathBuf::from(s))
}

#[test]
fn zip_mount_lists_root_children_only() {
    let dir = tmpdir("root");
    let z = dir.join("a.zip");
    make_zip(&z);

    let vol = ArchiveVolume::open(&z).unwrap();
    assert_eq!(vol.kind(), VolumeKind::Archive);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ents = rt
        .block_on(vol.list(&vp("archive:/"), &ListOpts::default()))
        .unwrap();

    let names: Vec<&str> = ents.iter().map(|e| e.name.as_str()).collect();
    // 根级只应有 readme.md、src、.hidden（默认隐藏过滤掉）
    assert!(
        names.contains(&"readme.md"),
        "应有 readme.md，实得 {names:?}"
    );
    assert!(names.contains(&"src"), "应有目录 src，实得 {names:?}");
    assert!(
        ents.iter().find(|e| e.name == "src").unwrap().is_dir,
        "src 必须是目录"
    );
    assert!(
        !names.contains(&"main.rs"),
        "孙级条目不应出现在根列表：{names:?}"
    );
    assert!(!names.contains(&"hidden"), "隐藏项默认不列：{names:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn zip_hidden_shows_when_requested() {
    let dir = tmpdir("hidden");
    let z = dir.join("a.zip");
    make_zip(&z);
    let vol = ArchiveVolume::open(&z).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ents = rt
        .block_on(vol.list(
            &vp("archive:/"),
            &ListOpts {
                show_hidden: true,
                sort: SortOrder::Name,
            },
        ))
        .unwrap();
    assert!(ents.iter().any(|e| e.name == ".hidden"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn zip_subdir_lists_only_its_children() {
    let dir = tmpdir("sub");
    let z = dir.join("a.zip");
    make_zip(&z);
    let vol = ArchiveVolume::open(&z).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ents = rt
        .block_on(vol.list(&vp("archive:/src"), &ListOpts::default()))
        .unwrap();
    let names: Vec<&str> = ents.iter().map(|e| e.name.as_str()).collect();
    // 目录排在文件前（与 LocalVolume 同口径），所以是 util 再 main.rs。
    assert_eq!(names, vec!["util", "main.rs"], "src 的直接子项应恰好是这些");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn zip_reads_member_content() {
    let dir = tmpdir("read");
    let z = dir.join("a.zip");
    make_zip(&z);
    let vol = ArchiveVolume::open(&z).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut s = rt
        .block_on(vol.read_stream(&vp("archive:/src/main.rs")))
        .unwrap();
    let mut acc = Vec::new();
    while let Some(chunk) = rt.block_on(s.next()) {
        acc.extend(chunk.unwrap());
    }
    assert_eq!(acc, b"fn main() {}\n", "读到的应是成员原文");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tar_synthesizes_missing_parent_dirs() {
    // tar 没有显式目录项：`docs/b/c.txt` 存在时，`docs` 与 `docs/b` 必须能列出来。
    let dir = tmpdir("tar");
    let t = dir.join("a.tar");
    make_tar(&t, false);
    let vol = ArchiveVolume::open(&t).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();

    let root = rt
        .block_on(vol.list(&vp("archive:/"), &ListOpts::default()))
        .unwrap();
    assert!(
        root.iter().any(|e| e.name == "docs" && e.is_dir),
        "docs 应作为合成目录出现"
    );
    let sub = rt
        .block_on(vol.list(&vp("archive:/docs"), &ListOpts::default()))
        .unwrap();
    let names: Vec<&str> = sub.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"a.txt") && names.contains(&"b"),
        "{names:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 关键回归：`.tar.gz` 的真身是 gzip，早先按扩展名喂纯解析器会打不开。
/// 现在靠魔数探测，必须能列能读。
#[test]
fn gzipped_tar_mounts_via_magic_probe() {
    let dir = tmpdir("tgz");
    let t = dir.join("a.tar.gz");
    make_tar(&t, true);
    // 先自证夹具真是 gzip
    let head = {
        let mut f = std::fs::File::open(&t).unwrap();
        let mut m = [0u8; 2];
        f.read_exact(&mut m).unwrap();
        m
    };
    assert_eq!(head, [0x1f, 0x8b], "夹具必须是 gzip 才有意义");

    let vol = ArchiveVolume::open(&t).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let root = rt
        .block_on(vol.list(&vp("archive:/"), &ListOpts::default()))
        .unwrap();
    assert!(root.iter().any(|e| e.name == "docs"), "{root:?}");

    let mut s = rt
        .block_on(vol.read_stream(&vp("archive:/docs/b/c.txt")))
        .unwrap();
    let mut acc = Vec::new();
    while let Some(chunk) = rt.block_on(s.next()) {
        acc.extend(chunk.unwrap());
    }
    assert_eq!(acc, b"CCC", "gzip 里的成员内容要解得对");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn archive_is_read_only() {
    let dir = tmpdir("ro");
    let z = dir.join("a.zip");
    make_zip(&z);
    let vol = ArchiveVolume::open(&z).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(!vol.writable());
    let e = rt.block_on(vol.write(&vp("archive:/x"), b"y")).unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "应报 Unsupported：{e}");
    let e = rt
        .block_on(vol.delete(&[vp("archive:/readme.md")], Arc::new(Progress::new())))
        .unwrap_err();
    assert!(
        matches!(e, Error::Unsupported(_)),
        "删除也要 Unsupported：{e}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn native_path_is_none_for_archive_members() {
    // 归档成员没有本地路径：交外部程序前必须先解出来，
    // 返回 Some(假路径) 会让上层把不存在的路径丢给系统。
    let dir = tmpdir("native");
    let z = dir.join("a.zip");
    make_zip(&z);
    let vol = ArchiveVolume::open(&z).unwrap();
    assert!(vol.native_path(&vp("archive:/readme.md")).is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_member_is_not_found_not_io_error() {
    // 与 LocalVolume 同口径：客户端给错路径 → NotFound，不是 500。
    let dir = tmpdir("miss");
    let z = dir.join("a.zip");
    make_zip(&z);
    let vol = ArchiveVolume::open(&z).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let e = rt
        .block_on(vol.list(&vp("archive:/nope"), &ListOpts::default()))
        .unwrap_err();
    assert!(
        matches!(e, Error::NotFound(_)),
        "列不存在目录应 NotFound：{e}"
    );
    // Box<dyn Stream> 不实现 Debug，所以不能用 unwrap_err()——手动 match。
    match rt.block_on(vol.read_stream(&vp("archive:/nope.txt"))) {
        Ok(_) => panic!("读不存在的成员应报错"),
        Err(e) => assert!(matches!(e, Error::NotFound(_)), "应 NotFound：{e}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn split_path_accepts_both_separators() {
    let (a, inner) = split_path(&PathBuf::from(r"archive:D:\x\a.zip/sub/dir")).unwrap();
    assert_eq!(a, PathBuf::from("D:/x/a.zip"));
    assert_eq!(inner, "sub/dir");

    let (a2, inner2) = split_path(&PathBuf::from(r"archive:D:\x\a.zip\sub\dir.txt")).unwrap();
    assert_eq!(a2, a, "反斜杠与正斜杠要归一到同一个归档");
    assert_eq!(inner2, "sub/dir.txt");

    // Windows 盘符里的冒号不能被当成前缀分隔符切掉
    let (a3, _) = split_path(&PathBuf::from("archive:C:/p/a.zip")).unwrap();
    assert_eq!(a3, PathBuf::from("C:/p/a.zip"), "盘符 C: 要留在归档路径里");
}

#[test]
fn split_path_rejects_non_archive_prefixes() {
    assert!(split_path(&PathBuf::from("/just/a/path")).is_err());
    assert!(split_path(&PathBuf::from("archive:C:/p/a.7z")).is_err());
}

#[test]
fn search_finds_nested_members_bounded() {
    let dir = tmpdir("search");
    let z = dir.join("a.zip");
    make_zip(&z);
    let vol = ArchiveVolume::open(&z).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let hits = rt
        .block_on(vol.search(&vp("archive:/"), "mod".into(), 10))
        .unwrap();
    assert_eq!(hits.len(), 1, "mod.rs 应命中一次：{hits:?}");
    assert!(hits[0].0.to_string_lossy().ends_with("src/util/mod.rs"));

    let limited = rt
        .block_on(vol.search(&vp("archive:/"), "rs".into(), 1))
        .unwrap();
    assert_eq!(limited.len(), 1, "limit 必须生效");
    std::fs::remove_dir_all(&dir).ok();
}
