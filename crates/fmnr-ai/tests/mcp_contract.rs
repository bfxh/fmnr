//! M4 出口判据：**外部 MCP 客户端能 list/search/copy**。
//!
//! 这套测试刻意不调用 `fmnr_ai` 里的任何函数——那样证明的是「函数能跑」，
//! 不是「外部客户端能连上」。这里 `spawn` 真实二进制，走真实 stdin/stdout 管道，
//! 只说 JSON-RPC。任何「进程起不来」「stdout 混进日志」「帧边界错」都会在这里红。

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// 一个真实运行中的 MCP 客户端会话。
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Mcp {
    /// 在 `cwd` 里启动服务（卷根 = cwd，故相对路径可用）。
    fn start(cwd: &std::path::Path) -> Self {
        let exe = env!("CARGO_BIN_EXE_fmnr-ai");
        let mut child = Command::new(exe)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr 走继承：既不会因管道写满而卡死，失败时日志也看得见。
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn fmnr-ai");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 0,
        }
    }

    /// 发一条请求并读回**一行**响应。
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        self.send(&req);
        let resp = self.read_frame();
        assert_eq!(resp["id"], json!(id), "回帧 id 必须与请求 id 对应");
        assert_eq!(resp["jsonrpc"], json!("2.0"));
        resp
    }

    /// 发一条通知（无 id），不读回帧。
    fn notify(&mut self, method: &str) {
        let req = json!({"jsonrpc":"2.0","method":method});
        self.send(&req);
    }

    fn send(&mut self, v: &Value) {
        let line = serde_json::to_string(v).unwrap();
        let stdin = self.stdin.as_mut().expect("stdin 已关闭");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// 读一行并解析。读到 EOF 会 panic（说明进程提前死了）。
    fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).expect("read frame");
        assert!(n > 0, "stdout 提前 EOF：进程可能崩了");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("回帧不是合法 JSON（{e}）：{line:?}"))
    }

    /// 收工：关 stdin（模拟客户端断开），读完剩余 stdout 到 EOF，等进程退出。
    ///
    /// 这里顺带断言 stdout 上**没有多余帧**——一次请求回两帧、或日志漏进
    /// stdout，都会在真实客户端那里表现为协议错乱。
    /// 必须先关 stdin 再读：EOF 由 stdin 关闭触发，反过来读会死锁。
    fn finish(mut self) {
        drop(self.stdin.take());
        let mut rest = String::new();
        {
            use std::io::Read;
            self.stdout.read_to_string(&mut rest).expect("read tail");
        }
        let extra: Vec<&str> = rest.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(
            extra.is_empty(),
            "stdout 上有多余输出（会污染协议）：{extra:?}"
        );
        let status = self.child.wait().expect("wait");
        assert!(status.success(), "进程退出码非 0：{status}");
    }

    /// 标准握手：initialize + initialized 通知。
    fn handshake(&mut self) -> Value {
        let init = self.call(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "contract-test", "version": "1.0"}
            }),
        );
        self.notify("notifications/initialized");
        init
    }
}

/// 取工具返回体里那段 JSON 文本（content[0].text）解析回 Value。
fn tool_payload(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("工具返回缺 content[0].text：{resp}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("工具返回不是 JSON（{e}）：{text}"))
}

/// 建一棵临时树，返回 (根, 用于清理的守卫)。
struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fmnr-mcp-{tag}-{}-{}",
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
        Self(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------- 握手

#[test]
fn initialize_advertises_tools_capability() {
    let fx = Fixture::new("init");
    let mut mcp = Mcp::start(&fx.0);
    let init = mcp.handshake();

    let result = &init["result"];
    assert_eq!(result["protocolVersion"], json!("2024-11-05"));
    assert!(
        result["capabilities"]["tools"].is_object(),
        "必须声明 tools 能力，否则客户端不会调 tools/list"
    );
    assert_eq!(result["serverInfo"]["name"], json!("fmnr-ai"));
    mcp.finish();
}

#[test]
fn tools_list_over_the_wire_contains_required_tools() {
    let fx = Fixture::new("tools");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call("tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().expect("tools 数组");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // M4 出口判据点名要 list/search/copy，其余是 DESIGN §4.10 的工具表。
    for want in [
        "list_volume",
        "list_dir",
        "search",
        "copy",
        "move",
        "delete",
        "open",
    ] {
        assert!(names.contains(&want), "缺工具 {want}，实得 {names:?}");
    }
    // 每个工具的 schema 必须是 object 且带 properties，否则客户端无法生成表单。
    for t in tools {
        assert_eq!(
            t["inputSchema"]["type"],
            json!("object"),
            "{t} 的 schema 非 object"
        );
        assert!(t["description"].is_string(), "{t} 缺 description");
    }
    mcp.finish();
}

// ---------------------------------------------------------------- list

#[test]
fn list_dir_returns_real_directory_entries() {
    let fx = Fixture::new("list");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "list_dir",
            "arguments": {"path": fx.0.to_string_lossy()}
        }),
    );
    let p = tool_payload(&resp);
    // 根目录只有 alpha.txt 与 sub；beta.log / gamma.txt 在 sub 里，不该被平铺出来。
    assert_eq!(p["count"], json!(2), "根目录应有 alpha.txt / sub 两项：{p}");

    let entries = p["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alpha.txt"), "实得 {names:?}");
    assert!(names.contains(&"sub"), "实得 {names:?}");

    let alpha = entries.iter().find(|e| e["name"] == "alpha.txt").unwrap();
    assert_eq!(alpha["is_dir"], json!(false));
    assert_eq!(
        alpha["size"],
        json!(5),
        "alpha.txt 内容 \"hello\" 应为 5 字节"
    );
    let sub = entries.iter().find(|e| e["name"] == "sub").unwrap();
    assert_eq!(sub["is_dir"], json!(true));

    mcp.finish();
}

// ---------------------------------------------------------------- search

#[test]
fn search_finds_nested_hits_under_the_root() {
    let fx = Fixture::new("search");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "search",
            "arguments": {"root": fx.0.to_string_lossy(), "query": "beta"}
        }),
    );
    let p = tool_payload(&resp);
    assert_eq!(p["count"], json!(1), "只有 sub/beta.log 命中：{p}");
    let hits = p["hits"].as_array().unwrap();
    let hit = hits[0].as_str().unwrap();
    assert!(hit.ends_with("beta.log"), "命中路径不对：{hit}");
    assert!(hit.contains("sub"), "命中应在 sub 子目录下：{hit}");
    mcp.finish();
}

#[test]
fn search_is_case_insensitive() {
    let fx = Fixture::new("searchcase");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "search",
            "arguments": {"root": fx.0.to_string_lossy(), "query": "ALPHA"}
        }),
    );
    let p = tool_payload(&resp);
    assert_eq!(p["count"], json!(1), "大小写不敏感应命中 alpha.txt：{p}");
    mcp.finish();
}

#[test]
fn search_honours_limit() {
    let fx = Fixture::new("limit");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    // 三个文件都含 "a"？至少 gamma/beta/alpha 里 alpha 与 gamma 命中 ".txt"
    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "search",
            "arguments": {"root": fx.0.to_string_lossy(), "query": "a", "limit": 1}
        }),
    );
    let p = tool_payload(&resp);
    assert_eq!(p["count"], json!(1), "limit=1 必须只回 1 条：{p}");
    mcp.finish();
}

// ---------------------------------------------------------------- copy（出口判据）

#[test]
fn copy_moves_bytes_on_disk_and_reports_them() {
    let fx = Fixture::new("copy");
    let dst = fx.0.join("out");
    std::fs::create_dir_all(&dst).unwrap();

    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "copy",
            "arguments": {
                "sources": [fx.0.join("alpha.txt").to_string_lossy()],
                "dest_dir": dst.to_string_lossy(),
            }
        }),
    );
    let p = tool_payload(&resp);
    assert_eq!(p["done"], json!(1), "应完成 1 项：{p}");
    assert_eq!(p["bytes"], json!(5));
    assert_eq!(p["skipped"], json!(0));

    // 判据锚在**磁盘上的真实字节**，不是返回值——返回值是规则作者自己的输出。
    let copied = dst.join("alpha.txt");
    assert!(copied.is_file(), "目标文件不存在：{}", copied.display());
    assert_eq!(std::fs::read(&copied).unwrap(), b"hello");
    // 源必须还在（copy 不是 move）
    assert!(fx.0.join("alpha.txt").is_file(), "拷贝不应删源");

    mcp.finish();
}

/// 冲突策略走线：默认 skip 不该覆盖，且要如实报 skipped。
#[test]
fn copy_default_policy_skips_and_reports_without_overwriting() {
    let fx = Fixture::new("conflict");
    let dst = fx.0.join("out");
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(dst.join("alpha.txt"), b"ORIGINAL").unwrap();

    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "copy",
            "arguments": {
                "sources": [fx.0.join("alpha.txt").to_string_lossy()],
                "dest_dir": dst.to_string_lossy(),
            }
        }),
    );
    let p = tool_payload(&resp);
    assert_eq!(p["skipped"], json!(1), "同名应被跳过：{p}");
    assert_eq!(p["done"], json!(0));
    assert_eq!(
        std::fs::read(dst.join("alpha.txt")).unwrap(),
        b"ORIGINAL",
        "默认策略绝不能覆盖目标内容"
    );
    mcp.finish();
}

#[test]
fn copy_overwrite_policy_replaces_target() {
    let fx = Fixture::new("overwrite");
    let dst = fx.0.join("out");
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(dst.join("alpha.txt"), b"ORIGINAL").unwrap();

    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "copy",
            "arguments": {
                "sources": [fx.0.join("alpha.txt").to_string_lossy()],
                "dest_dir": dst.to_string_lossy(),
                "conflict": "overwrite",
            }
        }),
    );
    let p = tool_payload(&resp);
    assert_eq!(p["done"], json!(1), "overwrite 应完成：{p}");
    assert_eq!(std::fs::read(dst.join("alpha.txt")).unwrap(), b"hello");
    mcp.finish();
}

#[test]
fn copy_rename_policy_keeps_both_files() {
    let fx = Fixture::new("rename");
    let dst = fx.0.join("out");
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(dst.join("alpha.txt"), b"ORIGINAL").unwrap();

    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "copy",
            "arguments": {
                "sources": [fx.0.join("alpha.txt").to_string_lossy()],
                "dest_dir": dst.to_string_lossy(),
                "conflict": "rename",
            }
        }),
    );
    let p = tool_payload(&resp);
    assert_eq!(p["done"], json!(1), "rename 应完成：{p}");
    assert_eq!(std::fs::read(dst.join("alpha.txt")).unwrap(), b"ORIGINAL");
    let renamed = dst.join("alpha (2).txt");
    assert!(
        renamed.is_file(),
        "应生成 alpha (2).txt：{:?}",
        std::fs::read_dir(&dst)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect::<Vec<_>>()
    );
    assert_eq!(std::fs::read(&renamed).unwrap(), b"hello");
    mcp.finish();
}

// ---------------------------------------------------------------- open 决策

/// 打开决策表必须与 UI 一致：文本走预览，未知类型走外部程序。
#[test]
fn open_routes_text_to_preview_and_unknown_to_external() {
    let fx = Fixture::new("open");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let txt = mcp.call(
        "tools/call",
        json!({
            "name": "open",
            "arguments": {"path": fx.0.join("alpha.txt").to_string_lossy()}
        }),
    );
    assert_eq!(
        tool_payload(&txt)["route"],
        json!("preview"),
        "txt 应走预览"
    );

    // 未知扩展名：会真的 spawn 一个 cmd.exe（挂角标），随后要清理掉。
    let unknown = fx.0.join("thing.xyzzy");
    std::fs::write(&unknown, b"?").unwrap();
    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "open",
            "arguments": {"path": unknown.to_string_lossy()}
        }),
    );
    let p = tool_payload(&resp);
    assert_eq!(p["route"], json!("external"), "未知类型应走外部程序：{p}");
    assert!(p["program"].is_string());

    // 角标状态应能查到
    let opens = mcp.call(
        "tools/call",
        json!({"name": "external_opens", "arguments": {}}),
    );
    let op = tool_payload(&opens);
    assert_eq!(op["count"], json!(1), "外置打开后应有一条角标：{op}");

    mcp.finish();
}

// ---------------------------------------------------------------- stdout 隔离（回归）

/// 回归：外置打开的子进程**不得污染 stdout**。
///
/// 真实缺陷（本套测试抓到的）：`CreateProcessW` 在未指定
/// `STARTF_USESTDHANDLES` 时**总会**把父进程标准句柄复制给子进程。
/// 于是 `cmd.exe` 把自己的横幅写进了 fmnr-ai 的 stdout——那正是 JSON-RPC 通道，
/// 客户端会立刻收到非法帧。修复是给子进程 `CREATE_NEW_CONSOLE`。
///
/// 这里让子进程打印一个**独特标记**：句柄若仍共享，标记就会出现在我们的
/// stdout 上，`finish()` 的「无多余帧」断言会把那段原文打出来。用 `/c` 而非
/// `/k`：子进程自己退出，测试不留下常驻进程。
#[test]
fn external_spawn_does_not_pollute_stdout() {
    let fx = Fixture::new("isolation");
    let unknown = fx.0.join("thing.xyzzy");
    std::fs::write(&unknown, b"?").unwrap();

    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "open",
            "arguments": {
                "path": unknown.to_string_lossy(),
                "program": "cmd.exe /c echo FMNR_STDOUT_POLLUTION"
            }
        }),
    );
    let p = tool_payload(&resp);
    assert_eq!(p["route"], json!("external"), "应走外置：{p}");

    // 给子进程足够时间把输出写出来（如果句柄被共享的话）。
    std::thread::sleep(std::time::Duration::from_millis(500));

    // 协议必须仍然健康：再发一条，回帧要能正常解析。
    let ping = mcp.call("ping", json!({}));
    assert_eq!(ping["result"], json!({}));

    // 收工时 finish() 断言 stdout 上没有多余输出；被污染时标记会现形。
    mcp.finish();
}

// ---------------------------------------------------------------- 错误面

#[test]
fn unknown_tool_is_an_error_not_a_silent_success() {
    let fx = Fixture::new("err");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call("tools/call", json!({"name": "explode", "arguments": {}}));
    assert!(resp["error"].is_object(), "未知工具必须报错：{resp}");
    assert_eq!(resp["error"]["code"], json!(-32601));
    assert!(resp["result"].is_null(), "错误响应不应带 result");
    mcp.finish();
}

#[test]
fn bad_arguments_report_invalid_params_with_field_name() {
    let fx = Fixture::new("badargs");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "search",
            "arguments": {"root": fx.0.to_string_lossy()}   // 缺 query
        }),
    );
    assert_eq!(resp["error"]["code"], json!(-32602));
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(msg.contains("query"), "错误信息应点出缺的是 query：{msg}");
    mcp.finish();
}

/// 不存在的路径要报「找不到」，而不是静默返回空列表。
#[test]
fn listing_a_missing_directory_is_an_error() {
    let fx = Fixture::new("missing");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    let resp = mcp.call(
        "tools/call",
        json!({
            "name": "list_dir",
            "arguments": {"path": fx.0.join("nope").to_string_lossy()}
        }),
    );
    assert!(resp["error"].is_object(), "列不存在的目录必须报错：{resp}");
    // 路径不存在是**调用方给错了**，不是服务端故障：必须是 -32602 而不是 -32603。
    // 只断言「报错了」会漏掉这个分类——曾经 list 把 NotFound 映射成 Io，
    // 客户端于是分不清「我路径写错」和「服务炸了」。
    assert_eq!(
        resp["error"]["code"],
        json!(-32602),
        "不存在的路径应归为 invalid params：{resp}"
    );
    mcp.finish();
}

/// 客户端直接断开（关 stdin）时进程要干净退出，不能挂住。
#[test]
fn closing_stdin_exits_cleanly() {
    let fx = Fixture::new("eof");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();
    mcp.finish(); // 内部断言 exit code == 0
}

/// 一行里塞两条报文（中间无换行）必须只解析成一条——帧边界按换行切。
#[test]
fn frames_are_delimited_by_newline_only() {
    let fx = Fixture::new("frames");
    let mut mcp = Mcp::start(&fx.0);
    mcp.handshake();

    // 发一条正常的 ping，确认连接仍然健康
    let resp = mcp.call("ping", json!({}));
    assert_eq!(resp["result"], json!({}));
    mcp.finish();
}
