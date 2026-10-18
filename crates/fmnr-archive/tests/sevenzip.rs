//! 7z 只读挂载：用**真 7-Zip 写出来的字节**建的门。
//!
//! 🔴 夹具不是我们自己那个库写的：`fixtures/*.7z` 由本机
//! `7-Zip (r) 26.00` 的 `7zr a -t7z -m0=LZMA2` 产出——
//! 用读它的那个库的写器造语料，测出来只是"它自洽"，不是"真界面上的 .7z 能挂"。
//!
//! ```text
//! fixtures/plain.7z  223 B  sha256 c05a61095c9d7a45d9ed4643e1e6b9f8b4b2d16bc3e2a72b15cea98c212cbcc8
//!     三条：hello.txt = "hello 7z\n"（9 B）· .hide = "dot\n"（4 B）
//!           · dir/deep.bin = "ABCDEFGHIJ"（10 B）← 头表里的原始名用的是**正斜杠**
//! fixtures/enc.7z    286 B  sha256 8d505db430eb6d29ac34dbfe3c6b1269d02ebd32a90089f6b24873abf6074350
//!     同一内容，多给 `-pS3cret -mhe=on`（头部与数据都加密）
//! ```
//! 嵌套那条钉的是"进子目录、按名取成员"这条通路。顺手实测到的口径（写在这儿免得下轮重猜）：
//! 7z 头表的分隔符是 `/`——`dir\deep.bin` 这种形态在本机 7-Zip 26.00 写的件、
//! 以及两个别人打包的现成件（91 + 3,156 条）里都没出现过；而 `read_file` 是**精确匹配**原始名的。
//! 重造：`7zr a -t7z -m0=LZMA2 out.7z .`（内容见上）。

use fmnr_archive::ArchiveVolume;
use fmnr_core::{Attr, IconKey, ListOpts, Result, VPath, Volume, VolumeKind};
use futures::StreamExt;
use std::path::{Path, PathBuf};

const PLAIN: &[u8] = include_bytes!("fixtures/plain.7z");
const ENC: &[u8] = include_bytes!("fixtures/enc.7z");

/// `read_stream` 里有 `tokio::task::spawn_blocking`，所以驱动它的必须是真 tokio
/// 运行时，`futures::executor::block_on` 会当场 panic（"no reactor running"）。
/// 与 `tests/archive_volume.rs` 同一口径。
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fmnr-7z-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write7z(tag: &str, bytes: &[u8]) -> PathBuf {
    let p = tmpdir(tag).join("a.7z");
    std::fs::write(&p, bytes).unwrap();
    p
}

async fn read(vol: &ArchiveVolume, key: &str) -> Result<Vec<u8>> {
    let mut s = vol.read_stream(&VPath(PathBuf::from(key))).await?;
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

fn shown(vol: &ArchiveVolume, hide: bool) -> Vec<String> {
    rt().block_on(vol.list(
        &VPath(PathBuf::from("archive:/")),
        &ListOpts {
            show_hidden: hide,
            ..Default::default()
        },
    ))
    .unwrap()
    .iter()
    .map(|e| e.name.clone())
    .collect()
}

/// 夹具没被改动成别的东西：7z 的签名头 + 登记过的长度。
#[test]
fn fixtures_still_are_the_bytes_7zip_wrote() {
    assert_eq!(PLAIN.len(), 223);
    assert_eq!(ENC.len(), 286);
    for b in [PLAIN, ENC] {
        assert_eq!(&b[..6], b"7z\xbc\xaf\x27\x1c", "7z 签名头不对");
    }
}

#[test]
fn mounts_lists_and_marks_hidden_the_same_way_as_other_volumes() {
    let p = write7z("list", PLAIN);
    let vol = ArchiveVolume::open(&p).expect("7-Zip 自己写的件必须挂得上");
    assert_eq!(vol.kind(), VolumeKind::Archive);

    let mut all = shown(&vol, true);
    all.sort();
    assert_eq!(all, vec![".hide", "dir", "hello.txt"], "头表里就这三条");
    // 默认（不显示隐藏）时 `.hide` 消失，而不是变成"这一层是空的"
    assert_eq!(shown(&vol, false), vec!["dir", "hello.txt"]);

    let entries = rt()
        .block_on(vol.list(
            &VPath(PathBuf::from("archive:/")),
            &ListOpts {
                show_hidden: true,
                ..Default::default()
            },
        ))
        .unwrap();
    let hello = entries.iter().find(|e| e.name == "hello.txt").unwrap();
    assert_eq!(hello.size, Some(9), "大小取自头表，不必解压");
    assert_eq!(hello.icon_key, IconKey::ByExtension("txt".into()));
    assert!(hello.modified.is_some(), "真件带修改时间，不该丢掉");
    let hide = entries.iter().find(|e| e.name == ".hide").unwrap();
    assert!(
        hide.attrs.contains(&Attr::Hidden),
        "点开头的名字要带 Hidden 标记（与本地/归档/远程同口径）"
    );
    let _ = Path::new(&p);
    std::fs::remove_dir_all(tmpdir("list")).ok();
}

#[test]
fn reads_members_byte_for_byte() {
    let p = write7z("read", PLAIN);
    let vol = ArchiveVolume::open(&p).unwrap();
    let r = rt();
    assert_eq!(
        r.block_on(read(&vol, "archive:/hello.txt")).unwrap(),
        b"hello 7z\n"
    );
    assert_eq!(r.block_on(read(&vol, "archive:/.hide")).unwrap(), b"dot\n");
    // 嵌套成员：走的是"key → 头表原始名"这条匹配。`read_file` 精确匹配原始名，
    // 分隔符差一个字符就报 FileNotFound（实测），所以这条既钉"能读"也钉"读对字节"。
    assert_eq!(
        r.block_on(read(&vol, "archive:/dir/deep.bin")).unwrap(),
        b"ABCDEFGHIJ"
    );
    // 目录项不能读
    let dir_err = r.block_on(read(&vol, "archive:/dir")).unwrap_err();
    assert!(
        matches!(dir_err, fmnr_core::Error::Unsupported(_)),
        "期望 Unsupported，拿到 {dir_err:?}"
    );
    // 不存在的成员要 NotFound，不能给空内容——空内容和"这是个空文件"长得一样
    let e = r.block_on(read(&vol, "archive:/nope.txt")).unwrap_err();
    assert!(
        matches!(e, fmnr_core::Error::NotFound(_)),
        "期望 NotFound，拿到 {e:?}"
    );
    std::fs::remove_dir_all(tmpdir("read")).ok();
}

#[test]
fn lists_a_nested_folder() {
    let p = write7z("nest", PLAIN);
    let vol = ArchiveVolume::open(&p).unwrap();
    let kids = rt()
        .block_on(vol.list(&VPath(PathBuf::from("archive:/dir")), &ListOpts::default()))
        .unwrap();
    assert_eq!(
        kids.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        vec!["deep.bin"]
    );
    assert_eq!(kids[0].size, Some(10));
    assert_eq!(kids[0].icon_key, IconKey::ByExtension("bin".into()));
    std::fs::remove_dir_all(tmpdir("nest")).ok();
}

/// 🔴 加密件：这一版没开 AES 特征、也没有输口令的入口 ⇒ 必须**明确失败**。
/// 挂上一个"看起来是空归档"的卷，等于把"我读不了"说成"这里面没东西"。
#[test]
fn encrypted_7z_fails_loudly_instead_of_mounting_empty() {
    let p = write7z("enc", ENC);
    let err = match ArchiveVolume::open(&p) {
        Ok(vol) => panic!("加密件挂上了：{:?}", shown(&vol, true)),
        Err(e) => e,
    };
    let msg = err.to_string();
    println!("加密件的报错原文：{msg}");
    assert!(
        msg.contains("AES") || msg.contains("加密") || msg.contains("不支持"),
        "报错要能看出是「这一版读不了这种编码/加密」，拿到的是：{msg}"
    );
    std::fs::remove_dir_all(tmpdir("enc")).ok();
}

/// 后缀是 `.7z` 但内容不是 7z ⇒ 报错；短到读不完签名也不能 panic。
#[test]
fn wrong_content_behind_the_extension_is_an_error_not_an_empty_volume() {
    let d = tmpdir("fake");
    for (name, body) in [
        ("f.7z", &b"just some bytes, definitely not a 7z header"[..]),
        ("s.7z", &b"7z"[..]),
        ("z.7z", &[] as &[u8]),
    ] {
        let p = d.join(name);
        std::fs::write(&p, body).unwrap();
        let r = std::panic::catch_unwind(|| ArchiveVolume::open(&p));
        assert!(r.is_ok(), "{name}: 打开过程 panic 了");
        assert!(r.unwrap().is_err(), "{name}: 内容不对却挂上了");
    }
    std::fs::remove_dir_all(&d).ok();
}
