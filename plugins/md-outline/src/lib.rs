//! md-outline：把 Markdown 的标题抽成大纲的预览插件。
//!
//! 这是 DESIGN §4.8「内置软件=预装插件」的一个**真插件**：编到
//! wasm32-unknown-unknown，由 fmnr 宿主加载，认领 `.md` 的预览渲染。
//!
//! `#![no_std]`：插件不需要标准库，也没有文件系统/网络可用（宿主不提供任何
//! import）。内存用一块静态 arena 手写 bump 分配，避免引入 allocator。

#![no_std]

use core::panic::PanicInfo;

/// 宿主 ABI 版本，必须与 `fmnr-plugins` 的 `ABI_VERSION` 一致。
pub const ABI_VERSION: i32 = 1;

/// 清单直接取 `plugin.json`，单一事实源；长度由编译器校验。
#[no_mangle]
#[link_section = "fmnr.plugin"]
pub static MANIFEST: [u8; 59] = *include_bytes!("../plugin.json");

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

const ARENA_SIZE: usize = 256 * 1024;
static mut ARENA: [u8; ARENA_SIZE] = [0; ARENA_SIZE];
static mut BUMP: usize = 0;

/// 从 arena 头部线性分配，返回**偏移**（不是地址）。
///
/// 偏移而非地址：宿主只关心 wasm 线性内存里的位置，偏移在这里算好后
/// 加上 arena 基址就是指针，少一层全局状态。
fn bump_alloc(len: usize) -> Option<usize> {
    unsafe {
        let bump = core::ptr::addr_of_mut!(BUMP);
        let cur = *bump;
        let end = cur.checked_add(len)?;
        if end > ARENA_SIZE {
            return None;
        }
        *bump = end;
        Some(cur)
    }
}

fn arena_base() -> *mut u8 {
    core::ptr::addr_of_mut!(ARENA) as *mut u8
}

/// ABI 版本。宿主加载时会调用并比对。
#[no_mangle]
pub extern "C" fn fmnr_abi_version() -> i32 {
    ABI_VERSION
}

/// 宿主把输入写进来之前先要一块地。失败返回 -1（宿主会明确拒绝负指针）。
#[no_mangle]
pub extern "C" fn fmnr_alloc(len: i32) -> i32 {
    if len < 0 {
        return -1;
    }
    match bump_alloc(len as usize) {
        Some(off) => (arena_base() as usize + off) as i32,
        None => -1,
    }
}

/// 渲染：`(ptr, len)` 是输入，返回 `(out_len << 32) | out_ptr`。
#[no_mangle]
pub extern "C" fn fmnr_render(ptr: i32, len: i32) -> i64 {
    if ptr < 0 || len < 0 {
        return -1;
    }
    let input = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };

    // 最坏情况：每行都是 6 级标题，输出前缀 12 空格 ⇒ 输出可能比输入长。
    // 留 2× 加余量，arena 用尽时如实返回 -1，不静默截断成错数据。
    let want = (len as usize).saturating_mul(2).saturating_add(64);
    let off = match bump_alloc(want) {
        Some(o) => o,
        None => return -1,
    };

    let cap = ARENA_SIZE - off;
    let out = unsafe { core::slice::from_raw_parts_mut(arena_base().add(off), cap) };
    let n = write_outline(input, out);

    let out_ptr = (arena_base() as usize + off) as i64;
    ((n as i64) << 32) | out_ptr
}

/// 抽取标题写成大纲。返回写入字节数。
///
/// 按 CommonMark 的 ATX 规则：`#` 之后**必须**跟空格或行尾才算标题。
/// 少了这条，`C#`、`#hashtag`、`#![no_std]` 都会被当成标题抽进大纲。
fn write_outline(input: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    for raw in input.split(|b| *b == b'\n') {
        let line = strip_cr(raw);
        let level = line.iter().take_while(|b| **b == b'#').count();
        if level == 0 || level > 6 {
            continue;
        }
        // 第 level 个字符必须是空格（或该行就是纯 `#`）。
        if line.len() > level && line[level] != b' ' {
            continue;
        }
        let title = trim(&line[level.min(line.len())..]);
        if title.is_empty() {
            continue;
        }
        n = push_repeat(out, n, b' ', level * 2);
        n = push_slice(out, n, title);
        n = push_slice(out, n, b"\n");
        if n >= out.len() {
            break;
        }
    }
    n
}

fn strip_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

fn push_slice(out: &mut [u8], n: usize, src: &[u8]) -> usize {
    let mut i = n;
    for &b in src {
        if i >= out.len() {
            return i;
        }
        out[i] = b;
        i += 1;
    }
    i
}

fn push_repeat(out: &mut [u8], n: usize, b: u8, times: usize) -> usize {
    let mut i = n;
    for _ in 0..times {
        if i >= out.len() {
            return i;
        }
        out[i] = b;
        i += 1;
    }
    i
}
