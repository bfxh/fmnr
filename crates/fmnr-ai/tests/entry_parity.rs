//! 三条入口对**同一能力**必须吐出逐字段相同的 JSON。
//!
//! 这条测试的存在理由：`docs/NEXT-BATCH.md` §6 纠正过一个前提——
//! MCP 并不比别家弱，三个入口（MCP stdio / HTTP / CLI）本来就共用
//! `tools::dispatch_payload`。但"共用一个函数"是**实现事实**，不是**对外契约**：
//! 每一层都可以各自包壳、各自改名、各自漏字段。这里把契约钉成可判红的东西：
//! 同一段参数走三条腿，回来的 JSON 必须**完全相等**（不是"关键字段相等"）。
//!
//! 覆盖的是本轮刚上表的四条：`create_dir` / `write_file` / `read_file` / `rename`。
//! 参数一律用**相对路径**——卷根就是各条腿自己的工作目录，所以返回体里回显的
//! `path` 字段天然与绝对路径无关，三条腿才可能字节级相等。

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

const TEXT: &str = "第一行\n";

/// 每条腿一棵独立的临时树：动作会改磁盘，共用一棵就会互相踩。
struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fmnr-parity-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Fixture(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            eprintln!("fixture 清理失败 {}：{e}", self.0.display());
        }
    }
}

/// 五个动作，按顺序，参数三条腿完全一致。
/// 返回每一步的输出 JSON；任何一步失败即 panic（带着是哪条腿）。
trait Leg {
    fn call(&mut self, tool: &str, args: Value) -> Value;

    fn run_sequence(&mut self) -> Vec<Value> {
        let steps: [(&str, Value); 5] = [
            ("create_dir", json!({"path": "out"})),
            ("write_file", json!({"path": "note.txt", "text": TEXT})),
            ("read_file", json!({"path": "note.txt"})),
            ("rename", json!({"from": "note.txt", "to": "out/note.txt"})),
            ("read_file", json!({"path": "out/note.txt"})),
        ];
        steps
            .iter()
            .map(|(tool, args)| self.call(tool, args.clone()))
            .collect()
    }
}

// -------------------------------------------------------------- 腿 1：MCP

struct Mcp {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Mcp {
    fn start(cwd: &Path) -> Self {
        let exe = env!("CARGO_BIN_EXE_fmnr-ai");
        let mut child = Command::new(exe)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn fmnr-ai");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut me = Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 0,
        };
        me.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "parity-test", "version": "1.0"}
            }),
        );
        me.notify("notifications/initialized");
        me
    }

    fn frame(&mut self, v: &Value) {
        let line = serde_json::to_string(v).unwrap();
        self.stdin
            .as_mut()
            .expect("stdin 已关")
            .write_all(line.as_bytes())
            .unwrap();
        self.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
        self.stdin.as_mut().unwrap().flush().unwrap();
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).expect("读一帧");
        assert!(n > 0, "stdout 提前 EOF：进程可能崩了");
        serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("非 JSON 回帧（{e}）：{line}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.frame(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        loop {
            let resp = self.read_line();
            // 通知帧可能插进来，只认同 id 的那一帧
            if resp["id"] == json!(id) {
                return resp;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        self.frame(&json!({"jsonrpc":"2.0","method":method}));
    }
}

impl Leg for Mcp {
    fn call(&mut self, tool: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": tool, "arguments": args}));
        assert!(
            resp["error"].is_null(),
            "MCP {tool} 报错：{}",
            resp["error"]
        );
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("MCP {tool} 返回缺 content[0].text：{resp}"));
        serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("MCP {tool} 载荷非 JSON（{e}）：{text}"))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

// -------------------------------------------------------------- 腿 2：HTTP

struct Http {
    port: u16,
    _guard: tokio::task::JoinHandle<()>,
}

impl Http {
    async fn start(root: &Path) -> Self {
        let volume = Arc::new(fmnr_fs::LocalVolume::new(root.to_path_buf()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = fmnr_ai::http::router(volume);
        let guard = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            port,
            _guard: guard,
        }
    }
}

impl Leg for Http {
    // 这条腿没法在 `&mut self` 里 block_on（测试本身跑在 tokio 运行时上），
    // 所以 HTTP 的序列在测试函数里单独 await，不走 run_sequence。
    fn call(&mut self, _tool: &str, _args: Value) -> Value {
        unreachable!("HTTP 腿用 http_call() 直接 await")
    }
}

async fn http_call(port: u16, tool: &str, args: Value) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = args.to_string();
    let req = format!(
        "POST /tool/{tool} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8(raw).expect("HTTP 响应应是 UTF-8");
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("拿不到状态码：{text}"));
    let payload = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    assert_eq!(status, 200, "HTTP {tool} 状态码不是 200，载荷：{payload}");
    serde_json::from_str(&payload)
        .unwrap_or_else(|e| panic!("HTTP {tool} 载荷非 JSON（{e}）：{payload}"))
}

// -------------------------------------------------------------- 腿 3：CLI

fn cli_bin() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_BIN_EXE_fmnr-ai"));
    let dir = me.parent().expect("二进制该在 target/debug 里");
    let name = if cfg!(windows) { "fmnr.exe" } else { "fmnr" };
    let p = dir.join(name);
    assert!(
        p.exists(),
        "找不到 CLI 二进制 {}：本测试要三条入口都在，请跑 `cargo test --workspace`（或先 `cargo build -p fmnr-cli`）",
        p.display()
    );
    p
}

/// 子命令的参数形状与工具名不一一对应（argv 本来就是另一种壳），
/// 所以这里显式给表；表本身也是"CLI 这条腿说的什么话"的记录。
struct Cli {
    cwd: PathBuf,
}

impl Leg for Cli {
    fn call(&mut self, tool: &str, args: Value) -> Value {
        let argv: Vec<String> = match tool {
            "create_dir" => vec!["mkdir".into(), args["path"].as_str().unwrap().into()],
            "write_file" => vec![
                "put".into(),
                args["path"].as_str().unwrap().into(),
                args["text"].as_str().unwrap().into(),
            ],
            "read_file" => vec!["cat".into(), args["path"].as_str().unwrap().into()],
            "rename" => vec![
                "rename".into(),
                args["from"].as_str().unwrap().into(),
                args["to"].as_str().unwrap().into(),
            ],
            other => panic!("CLI 腿没配 {other} 的子命令"),
        };
        let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        let out = Command::new(cli_bin())
            .args(&refs)
            .current_dir(&self.cwd)
            .output()
            .expect("启动 fmnr");
        assert!(
            out.status.success(),
            "CLI {refs:?} 应成功；stderr {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).expect("CLI stdout 应是 UTF-8");
        serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("CLI {refs:?} 的 stdout 不是 JSON（{e}）：{stdout}"))
    }
}

// ---------------------------------------------------------------- 判据

#[tokio::test]
async fn three_entries_return_identical_json_for_the_new_four_tools() {
    // 腿 1：MCP（真子进程 + 真 stdin/stdout 管道）
    let mfx = Fixture::new("mcp");
    let mut mcp = Mcp::start(&mfx.0);
    let mcp_out = mcp.run_sequence();
    drop(mcp);
    drop(mfx);

    // 腿 3：CLI（真子进程）
    let cfx = Fixture::new("cli");
    let mut cli = Cli { cwd: cfx.0.clone() };
    let cli_out = cli.run_sequence();
    drop(cli);
    drop(cfx);

    // 腿 2：HTTP（真套接字；在同一个 tokio 运行时里 await，故不走 Leg::call）
    let hfx = Fixture::new("http");
    let http = Http::start(&hfx.0).await;
    let http_out = [
        http_call(http.port, "create_dir", json!({"path": "out"})).await,
        http_call(
            http.port,
            "write_file",
            json!({"path": "note.txt", "text": TEXT}),
        )
        .await,
        http_call(http.port, "read_file", json!({"path": "note.txt"})).await,
        http_call(
            http.port,
            "rename",
            json!({"from": "note.txt", "to": "out/note.txt"}),
        )
        .await,
        http_call(http.port, "read_file", json!({"path": "out/note.txt"})).await,
    ];
    drop(http);
    drop(hfx);

    let tools = [
        "create_dir",
        "write_file",
        "read_file",
        "rename",
        "read_file",
    ];
    for i in 0..5 {
        assert_eq!(
            mcp_out[i],
            http_out[i],
            "第 {} 步（{}）MCP 与 HTTP 返回不同：\n MCP={} \n HTTP={}",
            i + 1,
            tools[i],
            mcp_out[i],
            http_out[i]
        );
        assert_eq!(
            mcp_out[i],
            cli_out[i],
            "第 {} 步（{}）MCP 与 CLI 返回不同：\n MCP={} \n CLI={}",
            i + 1,
            tools[i],
            mcp_out[i],
            cli_out[i]
        );
    }

    // 顺带钉住"内容真的动了磁盘"，否则三条腿一起返回空也算一致。
    assert_eq!(mcp_out[2]["text"], json!(TEXT));
    assert_eq!(mcp_out[2]["bytes"], json!(TEXT.len()));
    assert_eq!(mcp_out[2]["truncated"], json!(false));
    assert_eq!(mcp_out[2]["binary"], json!(false));
    assert_eq!(mcp_out[4]["path"], json!("out/note.txt"));
    assert!(mcp_out[0]["created"].is_boolean());
}
