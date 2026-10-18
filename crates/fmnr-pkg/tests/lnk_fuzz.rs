//! `.lnk` 是手写的字节解析器（`src/lnk.rs`），输入来自磁盘上任意一个快捷方式。
//!
//! 这把门钉两件事：
//! 1. 畸形件（截断、虚报长度、压根不是 lnk）必须变成 `LnkError`，不能变成 panic
//!    ——一个坏快捷方式不该带崩整个包管理器面板。
//! 2. 不能**编**：隔壁字节里看着像路径的串（旧实现就是从 StringData 那一格取的
//!    目标）在 LinkInfo 拿不到路径时必须判"没有目标"，而不是返回它。

use fmnr_pkg::lnk::{target_of, LnkError};
use std::panic::{catch_unwind, AssertUnwindSafe};

fn u32le(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}
fn u16le(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

fn wide(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

const CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

const F_IDLIST: u32 = 0x1;
const F_LINKINFO: u32 = 0x2;
const F_NAME: u32 = 0x4;
/// 0x40 是 HasIcon（**不是**"有 LocalBasePath"：目标只在 LinkInfo 里，
/// StringData 没有那一格）。这里只是拿它当额外的标志位噪声。
const F_ICON: u32 = 0x40;

/// 头 + 可选 IDList(声明 size) + 可选 LinkInfo(声明 size) + StringData。
/// 声明的长度**不需要**和真实字节数一致——这正是畸形件的做法：
/// 所以这里只写虚长的长度字段，实字节最多补 `PAD` 个。
/// （早先按声明长度真把字节填出来，探针自己申请了 8.59 GB 先崩了，
///  那是夹具的 bug，不是解析器的。）
const PAD: usize = 8;

fn build(flags: u32, idlist_size: u16, linkinfo_size: u32, strings: &[&str]) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(&u32le(76));
    d.extend_from_slice(&CLSID);
    d.extend_from_slice(&u32le(flags));
    d.resize(76, 0);
    if flags & F_IDLIST != 0 {
        d.extend_from_slice(&u16le(idlist_size));
        let real = (idlist_size as usize).min(PAD);
        d.extend_from_slice(&vec![0u8; real]);
    }
    if flags & F_LINKINFO != 0 {
        d.extend_from_slice(&u32le(linkinfo_size));
        let real = (linkinfo_size as usize).saturating_sub(4).min(PAD);
        d.extend_from_slice(&vec![0u8; real]);
    }
    for s in strings {
        let w = wide(s);
        d.extend_from_slice(&u16le(s.encode_utf16().count() as u16));
        d.extend_from_slice(&w);
    }
    d
}

fn quiet() {
    use std::sync::Once;
    static ON: Once = Once::new();
    ON.call_once(|| std::panic::set_hook(Box::new(|_| {})));
}

/// 1) 截断扫描：好件在每个长度上切一刀，任何一刀都不许 panic。
#[test]
fn truncation_sweep_never_panics() {
    quiet();
    let good = build(
        F_IDLIST | F_LINKINFO | F_NAME | F_ICON,
        6,
        28,
        &["A name", "C:\\Windows\\notepad.exe"],
    );
    let mut panicked = Vec::new();
    for n in 0..=good.len() {
        let cut = good[..n].to_vec();
        if catch_unwind(AssertUnwindSafe(|| {
            let _ = target_of(&cut);
        }))
        .is_err()
        {
            panicked.push(n);
        }
    }
    assert!(
        panicked.is_empty(),
        "截断到这些长度时 panic 了：{panicked:?}"
    );
}

/// 2) 畸形长度扫描：声明长度与实际字节无关的全组合。
#[test]
fn adversarial_sizes_never_panic() {
    quiet();
    let sizes16: [u16; 6] = [0, 1, 2, 3, u16::MAX, 0x4000];
    let sizes32: [u32; 7] = [0, 1, 2, 3, 4, 28, u32::MAX];
    let strs: [&[&str]; 2] = [
        &["x", "C:\\Windows\\notepad.exe"],
        &["只有一格", "只有一格"],
    ];
    let mut panicked = Vec::new();
    let mut checked = 0usize;
    for flags in 0..64u32 {
        for &a in &sizes16 {
            for &b in &sizes32 {
                for st in strs {
                    let d = build(flags, a, b, st);
                    checked += 1;
                    if catch_unwind(AssertUnwindSafe(|| {
                        let _ = target_of(&d);
                    }))
                    .is_err()
                    {
                        panicked.push((flags, a, b));
                    }
                }
            }
        }
    }
    assert!(checked > 5000, "探针自己没跑够：{checked}");
    assert!(panicked.is_empty(), "这些组合 panic 了：{panicked:?}");
}

/// 3) 随机字节：不是 lnk 的东西喂进去要得到错误，不是崩溃。
#[test]
fn random_bytes_never_panic() {
    quiet();
    let mut s = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut panicked = 0;
    for _ in 0..3000 {
        let len = (next() as usize) % 300;
        let mut d: Vec<u8> = Vec::with_capacity(len);
        for _ in 0..len {
            d.push((next() >> 24) as u8);
        }
        if catch_unwind(AssertUnwindSafe(|| {
            let _ = target_of(&d);
        }))
        .is_err()
        {
            panicked += 1;
        }
    }
    assert_eq!(panicked, 0, "随机字节把解析器打崩了 {panicked} 次");
}

/// 4) 反编造：整条绝对路径**只**存在于 LinkInfo 块后面的 StringData 里
///    （旧实现取的正是那一格，所以 230 个可读真件里有 155 个返回了错答案）。
///    LinkInfo 里一个偏移字段都没写对 ⇒ 必须判"没有目标"，不许拿隔壁那条当目标。
#[test]
fn never_invents_a_target_from_the_bytes_after_linkinfo() {
    // LinkInfo 声明 28 字节、实际补的字节全为 0 ⇒ 各偏移字段都是坏偏移
    let d = build(
        F_LINKINFO | F_NAME | F_ICON,
        0,
        28,
        &["C:\\Windows\\notepad.exe"],
    );
    let got = target_of(&d);
    println!("LinkInfo 全 0 + 后面跟着一条看着像目标的串 -> {got:?}");
    assert!(
        matches!(got, Err(LnkError::NoTarget)),
        "LinkInfo 里没目标却返回了 {got:?}——那只能是从隔壁字节里编的"
    );
}
