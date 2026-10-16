//! CLI 端到端：跑**真二进制**，断言 stdout 是真 JSON 且落盘动作真的发生。
//!
//! 这层不重复测参数映射（`main.rs` 单测已覆盖），只验「装配」：
//! 子命令 → dispatch → 卷 → 磁盘。任何一处接线断了这里都会红。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 每个用例一个独立临时目录，避免并行互相踩。
struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fmnr-cli-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("alpha.txt"), b"hello").unwrap();
        std::fs::write(root.join("sub/beta.log"), b"world").unwrap();
        std::fs::write(root.join("sub/gamma.txt"), b"!").unwrap();
        Fixture(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// 跑 CLI；`cwd` 作为进程工作目录（决定卷根与相对路径解析）。
fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fmnr"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("启动 fmnr")
}

/// 跑成功路径并解析 stdout 的 JSON。
fn run_json(cwd: &Path, args: &[&str]) -> serde_json::Value {
    let out = run(cwd, args);
    assert!(
        out.status.success(),
        "fmnr {args:?} 应成功，实得 {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 必须是 UTF-8");
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout 不是 JSON：{e}\n原文：{stdout}"))
}

#[test]
fn volumes_reports_a_local_volume() {
    let fx = Fixture::new("vols");
    let p = run_json(&fx.0, &["volumes"]);
    let vols = p["volumes"].as_array().expect("volumes 应是数组");
    assert_eq!(vols.len(), 1, "应恰好一个本地卷：{p}");
    assert_eq!(vols[0]["kind"], serde_json::json!("Local"));
}

#[test]
fn ls_lists_real_entries() {
    let fx = Fixture::new("ls");
    let p = run_json(&fx.0, &["ls", "."]);
    // 根目录只有 alpha.txt 与 sub
    assert_eq!(p["count"], serde_json::json!(2), "实得 {p}");
    let names: Vec<&str> = p["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alpha.txt"), "实得 {names:?}");
    assert!(names.contains(&"sub"), "实得 {names:?}");
}

#[test]
fn search_finds_nested_hit_case_insensitively() {
    let fx = Fixture::new("search");
    let p = run_json(&fx.0, &["search", ".", "BETA"]);
    assert_eq!(p["count"], serde_json::json!(1), "应命中 sub/beta.log：{p}");
    let hit = p["hits"][0].as_str().unwrap();
    assert!(hit.ends_with("beta.log"), "命中路径不对：{hit}");
}

#[test]
fn cp_moves_bytes_on_disk() {
    let fx = Fixture::new("cp");
    let dst = fx.0.join("dest");
    std::fs::create_dir_all(&dst).unwrap();

    let p = run_json(&fx.0, &["cp", "alpha.txt", "--to", dst.to_str().unwrap()]);
    assert_eq!(p["done"], serde_json::json!(1), "{p}");
    assert_eq!(p["bytes"], serde_json::json!(5), "应搬运 5 字节：{p}");

    // 断言磁盘真状态，而不是只看返回值
    assert_eq!(std::fs::read(dst.join("alpha.txt")).unwrap(), b"hello");
    assert!(fx.0.join("alpha.txt").is_file(), "拷贝不应删源");
}

#[test]
fn cp_conflict_skip_does_not_overwrite() {
    let fx = Fixture::new("cpskip");
    let dst = fx.0.join("dest");
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(dst.join("alpha.txt"), b"ORIGINAL").unwrap();

    let p = run_json(&fx.0, &["cp", "alpha.txt", "--to", dst.to_str().unwrap()]);
    assert_eq!(p["skipped"], serde_json::json!(1), "默认策略应跳过：{p}");
    assert_eq!(
        std::fs::read(dst.join("alpha.txt")).unwrap(),
        b"ORIGINAL",
        "跳过不应覆盖目标"
    );
}

/// 未知子命令要非零退出，且不能把错误混进 stdout。
#[test]
fn unknown_subcommand_exits_nonzero() {
    let fx = Fixture::new("badcmd");
    let out = run(&fx.0, &["frobnicate"]);
    assert!(!out.status.success(), "未知子命令应非零退出");
    assert!(out.stdout.is_empty(), "错误不该写进 stdout");
}

/// 缺必填参数（search 少了 query）要被 clap 拦在入口，非零退出。
#[test]
fn missing_required_arg_exits_nonzero() {
    let fx = Fixture::new("missing");
    let out = run(&fx.0, &["search", "."]);
    assert!(!out.status.success(), "缺 query 应非零退出");
    let err = String::from_utf8_lossy(&out.stderr);
    // clap 把占位符渲染成大写 <QUERY>，故比较时统一小写。
    assert!(
        err.to_lowercase().contains("query"),
        "错误信息应提到 query：{err}"
    );
}

/// 列一个不存在的目录：工具层报错 → stderr + 退出码 1，stdout 干净。
#[test]
fn listing_missing_dir_fails_cleanly() {
    let fx = Fixture::new("nope");
    let out = run(&fx.0, &["ls", "definitely-not-here"]);
    assert!(!out.status.success(), "不存在的目录应报错");
    assert!(
        out.stdout.is_empty(),
        "错误时 stdout 应为空：{:?}",
        out.stdout
    );
    assert!(!out.stderr.is_empty(), "错误信息应出现在 stderr");
}

/// `register --dry-run` 只打印计划，**不写注册表**。
/// e2e 里刻意只用 dry-run：真写会污染跑测试那台机器的注册表。
#[test]
fn register_dry_run_prints_plan_without_writing() {
    let fx = Fixture::new("regdry");
    let p = run_json(&fx.0, &["register", "--dry-run"]);

    let ops = p["will_write"].as_array().expect("应有 will_write");
    assert!(!ops.is_empty(), "计划不该为空");

    // 每一条都必须落在 HKCU\Software\Classes 下
    for op in ops {
        let key = op["key"].as_str().unwrap();
        assert!(
            key.starts_with("HKCU\\Software\\Classes\\"),
            "越界写到 {key}：只允许 HKCU\\Software\\Classes"
        );
    }

    // 共享键必须走「只删自己的值」那条路，不能进整键删除列表
    let owned = p["owned_keys_we_would_delete"]
        .as_array()
        .expect("应有 owned_keys");
    for ext in [".txt", ".md", ".log"] {
        let shared = format!("Software\\Classes\\{ext}\\OpenWithProgids");
        assert!(
            !owned.iter().any(|k| k.as_str() == Some(&shared)),
            "{shared} 进了整键删除列表——会误删别人的打开方式登记"
        );
    }

    // 协议标记必须出现，否则 fmnr:// 不被 Windows 认作协议处理器
    let has_url_protocol = ops.iter().any(|o| o["name"] == "URL Protocol");
    assert!(has_url_protocol, "计划里缺少 URL Protocol 标记");
}

/// 干跑不落盘：跑完之后注册表里不该多出 fmnr 键。
/// 这条用 `reg query` 独立核对，不看程序自己的输出。
///
/// **`#[cfg(windows)]`**：见证者是 `reg.exe` 与 HKCU 注册表本身，Linux 上没有
/// 对应物（`Command::new("reg")` 会直接 panic 在 `.expect()`，CI ubuntu 就是这么红的）。
/// 注意上面那条 `register_dry_run_prints_plan_without_writing` **刻意不门控**：
/// 它验的是「计划只落 HKCU\Software\Classes、共享键不进整键删除列表」这条
/// **安全不变量**，而计划是 `RegistrationPlan::build` 生成的平台无关数据
/// （只有 `apply`/`revert` 带 cfg），所以这条约束在两个平台上都得判红得了。
#[cfg(windows)]
#[test]
fn register_dry_run_leaves_registry_untouched() {
    let fx = Fixture::new("regnoop");
    let _ = run(&fx.0, &["register", "--dry-run"]);

    // 独立的见证者：reg.exe，不是 fmnr 自己的说法
    let out = Command::new("reg")
        .args(["query", "HKCU\\Software\\Classes\\fmnr"])
        .output()
        .expect("reg query");
    assert!(
        !out.status.success(),
        "干跑后不该存在 HKCU\\Software\\Classes\\fmnr，但 reg query 找到了"
    );
}

/// 新上表的四条能力走完整 argv → 分发 → 磁盘 这条路。
/// 单测（tools.rs 里）只证明分发表认得它们；这条证明**命令行真能用**，
/// 也正是"内核有但入口调不到"那个缺口的验收。
#[test]
fn put_cat_mkdir_rename_roundtrip_through_argv() {
    let fx = Fixture::new("newtools");
    let target = fx.0.join("note.txt");

    let p = run_json(
        &fx.0,
        &[
            "put",
            "note.txt",
            "第一行
",
        ],
    );
    assert_eq!(p["bytes"], serde_json::json!(10), "UTF-8 中文字节数：{p}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "第一行
"
    );

    let p = run_json(&fx.0, &["cat", "note.txt"]);
    assert_eq!(
        p["text"],
        serde_json::json!(
            "第一行
"
        )
    );
    assert_eq!(p["binary"], serde_json::json!(false));
    assert_eq!(p["truncated"], serde_json::json!(false));

    run_json(&fx.0, &["mkdir", "made"]);
    assert!(fx.0.join("made").is_dir(), "mkdir 应真建出目录");

    run_json(&fx.0, &["rename", "note.txt", "made/renamed.txt"]);
    assert!(!target.exists(), "改名后旧路径不该还在");
    assert!(fx.0.join("made").join("renamed.txt").exists());
}

/// 上限要能在命令行上表达，且**截断必须说出来**——
/// 客户端拿半截内容当全文再写回去会毁文件。
#[test]
fn cat_reports_truncation_from_cli() {
    let fx = Fixture::new("catcut");
    std::fs::write(fx.0.join("big.txt"), vec![b'z'; 400]).unwrap();
    let p = run_json(&fx.0, &["cat", "big.txt", "--max-bytes", "50"]);
    assert_eq!(p["bytes"], serde_json::json!(50));
    assert_eq!(p["truncated"], serde_json::json!(true), "必须报截断：{p}");
}
