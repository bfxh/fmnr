//! M5 验收：装一个自定义预览插件并生效（DESIGN §10）。
//!
//! 这里的插件是**真的**：`plugins/md-outline` 用 Rust 编到
//! wasm32-unknown-unknown，产物随仓库分发。测试直接加载那个分发件——
//! 手写 WAT 能过不代表真插件能过（真插件要过 linker 的 arena 分配、
//! `#[link_section]` 清单、panic 处理这些 Rust 特有的路径）。

use fmnr_plugins::registry::PluginSet;
use std::path::{Path, PathBuf};

/// 仓库里的真插件分发件。
fn plugin_wasm() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/md-outline/md-outline.wasm")
}

/// 每个测试用**独立**目录：同进程内测试是并行跑的，共用一个目录会让
/// 一个测试的 `remove_dir_all` 撞上另一个的 `copy`（Windows 直接报文件占用）。
fn load_real_plugin() -> PluginSet {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);

    let dir = std::env::temp_dir().join(format!("fmnr-real-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(plugin_wasm(), dir.join("md-outline.wasm"))
        .expect("真插件分发件应存在；先跑 plugins/md-outline 的构建");

    let (set, warns) = PluginSet::load_dir(&dir);
    assert!(
        warns.is_empty(),
        "真插件应干净加载，实际有警告：{:?}",
        warns.iter().map(|w| &w.error).collect::<Vec<_>>()
    );
    set
}

#[test]
fn real_rust_plugin_loads_with_its_manifest() {
    let set = load_real_plugin();
    assert_eq!(set.len(), 1, "应加载到 1 个插件");

    let m = set.manifests()[0];
    assert_eq!(m.name, "md-outline");
    assert_eq!(m.version, "1.0.0");
    assert_eq!(m.extensions, vec!["md"]);
}

#[test]
fn real_plugin_claims_md_and_renders_an_outline() {
    let mut set = load_real_plugin();
    assert!(set.handles("md"), "插件应认领 md");
    assert!(!set.handles("txt"), "插件不该越权认领 txt");

    let md = b"# Title\n\nsome text\n\n## Section\n### Deep\n";
    let out = set
        .render("md", md)
        .expect("md 应被插件认领")
        .expect("渲染应成功");
    let text = String::from_utf8(out).expect("输出应是 UTF-8");

    // 2 空格 × 级别缩进，标题文字保留。
    assert!(text.contains("  Title"), "一级标题缩进 2 空格：{text:?}");
    assert!(
        text.contains("    Section"),
        "二级标题缩进 4 空格：{text:?}"
    );
    assert!(text.contains("      Deep"), "三级标题缩进 6 空格：{text:?}");
    assert!(
        !text.contains("some text"),
        "正文不该出现在大纲里：{text:?}"
    );
}

/// `C#` 与 `#hashtag` 不是标题——CommonMark 要求 `#` 后跟空格。
/// 少了这条判定，代码块里的 `#![no_std]` 会被抽进大纲。
#[test]
fn real_plugin_does_not_treat_hash_without_space_as_heading() {
    let mut set = load_real_plugin();
    let md = b"C# is a language\n#hashtag\n#![no_std]\n# Real Heading\n";
    let out = set.render("md", md).unwrap().unwrap();
    let text = String::from_utf8(out).unwrap();

    assert!(text.contains("Real Heading"), "真标题应被抽出：{text:?}");
    assert!(!text.contains("hashtag"), "`#hashtag` 不是标题：{text:?}");
    assert!(!text.contains("no_std"), "`#![no_std]` 不是标题：{text:?}");
    assert!(!text.contains("C#"), "`C#` 不是标题：{text:?}");
}

#[test]
fn real_plugin_handles_crlf_and_empty_input() {
    let mut set = load_real_plugin();
    // CRLF：Windows 上写的 .md 到处都是，`\r` 不能进标题文字。
    let out = set.render("md", b"# Win\r\n## Sub\r\n").unwrap().unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains('\r'), "输出不该带 CR：{text:?}");
    assert!(text.contains("Win"));

    let empty = set.render("md", b"").unwrap().unwrap();
    assert!(empty.is_empty(), "空输入应产出空大纲");
}

/// 插件不该认领的扩展名：宿主返回 None，让调用方回落内置预览。
#[test]
fn real_plugin_leaves_other_extensions_alone() {
    let mut set = load_real_plugin();
    assert!(set.render("txt", b"# not markdown").is_none());
    assert!(set.render("rs", b"// code").is_none());
}
