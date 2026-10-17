//! MCP stdio 传输：一行一条 JSON-RPC 消息（newline-delimited JSON）。
//!
//! 选 stdio 而非 SSE/HTTP 是 MCP 的默认传输，客户端（Claude Desktop、
//! Qoder 等）直接 spawn 本进程并把管道接上，不需要端口与鉴权。
//!
//! 关键纪律：
//! - **stdout 只走协议**。任何日志/调试输出必须去 stderr，否则会污染报文流。
//! - 一行一帧；读到 EOF 就干净退出（客户端关管道即结束）。

use fmnr_core::volreg::VolumeRegistry;
use fmnr_core::Volume;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::Arc;

use crate::protocol::*;
use crate::tools;

/// 本服务声明的 MCP 协议版本。
pub const PROTOCOL_VERSION: &str = "2024-11-05";

pub struct Server {
    /// 卷**注册表**而不是单个卷：谁起这个服务谁往里填，填几个就能路由几个。
    /// 路由规则在 `fmnr-core::volreg`，与 CLI 共用一份（见那里的 `route_or_default`）。
    registry: Arc<VolumeRegistry>,
    /// 客户端在 initialize 里报的版本，原样回给 tools/list 之后的分支判断。
    client_version: Option<String>,
    initialized: bool,
}

impl Server {
    /// 单卷起服务（老调用方与测试用）。
    pub fn new(volume: Arc<dyn Volume>) -> Self {
        let registry = VolumeRegistry::new();
        registry.add(volume);
        Self::with_registry(Arc::new(registry))
    }

    /// 带注册表起服务：要让更多卷被看见，调用方自己挂好再交进来。
    /// 这里刻意**不**自己去挂归档/远程卷——那会把 `zstd-sys`/`ring` 的跨端 C 依赖
    /// 拽进 MCP server，代价与取舍写在 `docs/NEXT-BATCH.md` §17。
    pub fn with_registry(registry: Arc<VolumeRegistry>) -> Self {
        Self {
            registry,
            client_version: None,
            initialized: false,
        }
    }

    /// 处理一条已解析的请求。返回 None 表示这是通知（不该回包）。
    pub async fn handle(&mut self, req: Request) -> Option<Response> {
        let id = req.id.clone();
        let result = self.route(&req).await;

        if req.is_notification() {
            // 通知不回包；但错误不能吞——打到 stderr 便于诊断。
            if let Err(e) = result {
                eprintln!("fmnr-ai: 通知 {} 处理出错：{}", req.method, e.message);
            }
            return None;
        }

        let id = id.unwrap_or(Value::Null);
        Some(match result {
            Ok(v) => Response::ok(id, v),
            Err(e) => Response::err(id, e),
        })
    }

    async fn route(&mut self, req: &Request) -> Result<Value, RpcError> {
        match req.method.as_str() {
            "initialize" => {
                let params = req.params.clone().unwrap_or(json!({}));
                self.client_version = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                // 回我们**自己的**版本，不是回声客户端版本：客户端据此决定降级。
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "fmnr-ai",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }))
            }

            "notifications/initialized" | "initialized" => {
                self.initialized = true;
                Ok(json!({}))
            }

            "ping" => Ok(json!({})),

            "tools/list" => {
                let list: Vec<Value> = tools::specs()
                    .into_iter()
                    .map(|s| {
                        json!({
                            "name": s.name,
                            "description": s.description,
                            "inputSchema": s.input_schema,
                        })
                    })
                    .collect();
                Ok(json!({ "tools": list }))
            }

            "tools/call" => {
                let params = req.params.clone().unwrap_or(json!({}));
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| RpcError::new(INVALID_PARAMS, "缺少 tools/call 的 name"))?
                    .to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                // 按参数选卷，选不上回退注册表里的第一个（规则与 CLI 同一份）。
                let volume = self
                    .registry
                    .route_or_default(&args)
                    .ok_or_else(|| RpcError::new(INTERNAL_ERROR, "没有已挂载的卷：注册表是空的"))?;
                tools::dispatch(volume, &name, args).await
            }

            other => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("未实现的方法：{other}"),
            )),
        }
    }

    /// 跑 stdio 主循环，直到输入 EOF。
    pub async fn serve<R: BufRead, W: Write>(
        &mut self,
        mut input: R,
        mut out: W,
    ) -> std::io::Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = input.read_line(&mut line)?;
            if n == 0 {
                break; // EOF：客户端关管道，正常收工
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue; // 空行不是消息，跳过（不报错，保持兼容）
            }

            // 批量请求：MCP 已移除该语义，明确报错而不是按单条处理。
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    write_frame(
                        &mut out,
                        &Response::err(
                            Value::Null,
                            RpcError::new(PARSE_ERROR, format!("JSON 解析失败：{e}")),
                        ),
                    )?;
                    continue;
                }
            };
            if value.is_array() {
                write_frame(
                    &mut out,
                    &Response::err(
                        Value::Null,
                        RpcError::new(
                            INVALID_REQUEST,
                            "不支持批量请求（JSON-RPC 2.0 batch 已从 MCP 移除）",
                        ),
                    ),
                )?;
                continue;
            }

            let req = match Request::from_value(value) {
                Ok(r) => r,
                Err(e) => {
                    write_frame(&mut out, &Response::err(Value::Null, e))?;
                    continue;
                }
            };

            if let Some(resp) = self.handle(req).await {
                write_frame(&mut out, &resp)?;
            }
        }
        Ok(())
    }
}

/// 写一帧：紧凑 JSON + 换行，立刻 flush（客户端可能一直等着读）。
fn write_frame<W: Write>(out: &mut W, resp: &Response) -> std::io::Result<()> {
    let s = serde_json::to_string(resp).unwrap_or_else(|e| {
        format!(
            r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":{},"message":"序列化失败：{e}"}}}}"#,
            INTERNAL_ERROR
        )
    });
    out.write_all(s.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fmnr_core::VolumeId;
    use fmnr_fs::LocalVolume;

    fn server() -> Server {
        Server::new(Arc::new(LocalVolume::new(std::env::temp_dir())))
    }

    /// **注册表路由**：路径带着第二个卷的 id 前缀时，必须真的把这次调用交给
    /// 第二个卷，而不是回退到"注册表第一个"。
    ///
    /// 为什么用探针卷而不是两个本地卷：`LocalVolume` 本来就接受绝对路径，
    /// 所以"走错卷"在本地卷之间**看不出差别**——第一版这条测试就是这么假绿的
    /// （把路由整个摘掉它照样过）。探针卷的 `list` 各自回一个只有它自己有的
    /// 标记名，选错卷立刻显形。
    #[tokio::test]
    async fn tools_call_routes_to_the_matching_volume_not_the_first_one() {
        let reg = VolumeRegistry::new();
        let id_a = reg.add(Arc::new(ProbeVol::new("probe:alpha")));
        let id_b = reg.add(Arc::new(ProbeVol::new("probe:beta")));
        assert_ne!(id_a, id_b, "两个探针卷必须有不同 id，否则这条测试没有判据");
        let mut s = Server::with_registry(Arc::new(reg));

        let input = format!(
            "{}\n{}\n{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":PROTOCOL_VERSION,"capabilities":{},
                "clientInfo":{"name":"t","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"list_dir","arguments":{"path": format!("{}/inner", id_a.0)}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"list_dir","arguments":{"path": format!("{}/inner", id_b.0)}}}),
        );
        let got = frames(&mut s, &input).await;
        let marker = |id: i64| -> String {
            let text = got
                .iter()
                .find(|f| f["id"] == json!(id))
                .unwrap_or_else(|| panic!("没拿到 id={id} 的回帧：{got:?}"))
                .pointer("/result/content/0/text")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("id={id} 没有 text 载荷：{got:?}"))
                .to_string();
            let v: Value = serde_json::from_str(&text).unwrap();
            v["entries"][0]["name"]
                .as_str()
                .unwrap_or("<空>")
                .to_string()
        };
        assert_eq!(marker(2), "来自-alpha", "第 2 帧走错卷");
        assert_eq!(marker(3), "来自-beta", "第 3 帧走错卷");
    }

    /// 注册表空着时必须说"没有已挂载的卷"，而不是 panic 或回一个空清单
    /// （空清单会被客户端读成"这个目录是空的"）。
    #[tokio::test]
    async fn empty_registry_says_so_instead_of_listing_nothing() {
        let mut s = Server::with_registry(Arc::new(VolumeRegistry::new()));
        let got = frames(
            &mut s,
            &format!(
                "{}\n{}\n",
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":PROTOCOL_VERSION,"capabilities":{},
                    "clientInfo":{"name":"t","version":"1"}}}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                    "name":"list_dir","arguments":{"path":"."}}}),
            ),
        )
        .await;
        let err = got
            .iter()
            .find(|f| f["id"] == json!(2))
            .and_then(|f| f.get("error"))
            .unwrap_or_else(|| panic!("空注册表应当报错，实得 {got:?}"));
        assert!(
            err["message"]
                .as_str()
                .unwrap_or_default()
                .contains("没有已挂载的卷"),
            "错误文案要说清是没卷：{err}"
        );
    }

    /// 只实现这条测试要用的 `id`/`list`，其余一律 panic：
    /// 一旦哪天 `tools/call` 走了别的方法，这里会立刻炸出来，
    /// 比默默给个假实现更容易看清测试在测什么。
    struct ProbeVol {
        id: VolumeId,
        marker: &'static str,
        opens: fmnr_core::ExternalOpens,
    }

    impl ProbeVol {
        fn new(id: &str) -> Self {
            let marker = if id.ends_with("alpha") {
                "来自-alpha"
            } else {
                "来自-beta"
            };
            Self {
                id: VolumeId(id.into()),
                marker,
                opens: fmnr_core::ExternalOpens::new(),
            }
        }
        fn nope(&self) -> ! {
            panic!("探针卷不该被问到这个方法（说明测试走偏了）");
        }
    }

    #[async_trait::async_trait]
    impl Volume for ProbeVol {
        fn id(&self) -> VolumeId {
            self.id.clone()
        }
        fn kind(&self) -> fmnr_core::VolumeKind {
            fmnr_core::VolumeKind::Local
        }
        async fn list(
            &self,
            _path: &fmnr_core::VPath,
            _opts: &fmnr_core::ListOpts,
        ) -> fmnr_core::Result<Vec<fmnr_core::Entry>> {
            Ok(vec![fmnr_core::Entry {
                name: self.marker.to_string(),
                is_dir: false,
                size: Some(1),
                modified: None,
                icon_key: fmnr_core::IconKey::Generic,
                attrs: Vec::new(),
            }])
        }
        async fn read_stream(
            &self,
            _: &fmnr_core::VPath,
        ) -> fmnr_core::Result<
            Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>,
        > {
            self.nope()
        }
        async fn write(&self, _: &fmnr_core::VPath, _: &[u8]) -> fmnr_core::Result<()> {
            self.nope()
        }
        fn writable(&self) -> bool {
            false
        }
        fn native_path(&self, _: &fmnr_core::VPath) -> Option<std::path::PathBuf> {
            None
        }
        async fn spawn_external_open(
            &self,
            _: &fmnr_core::VPath,
            _: &str,
        ) -> fmnr_core::Result<fmnr_core::ExternalOpen> {
            self.nope()
        }
        fn external_opens(&self) -> &fmnr_core::ExternalOpens {
            &self.opens
        }
        async fn copy(
            &self,
            _: &[fmnr_core::VPath],
            _: &fmnr_core::VPath,
            _: fmnr_core::ConflictPolicy,
            _: Arc<fmnr_core::Progress>,
        ) -> fmnr_core::Result<fmnr_core::OpReport> {
            self.nope()
        }
        async fn move_entries(
            &self,
            _: &[fmnr_core::VPath],
            _: &fmnr_core::VPath,
            _: fmnr_core::ConflictPolicy,
            _: Arc<fmnr_core::Progress>,
        ) -> fmnr_core::Result<fmnr_core::OpReport> {
            self.nope()
        }
        async fn delete(
            &self,
            _: &[fmnr_core::VPath],
            _: Arc<fmnr_core::Progress>,
        ) -> fmnr_core::Result<fmnr_core::OpReport> {
            self.nope()
        }
        async fn rename(
            &self,
            _: &fmnr_core::VPath,
            _: &fmnr_core::VPath,
        ) -> fmnr_core::Result<()> {
            self.nope()
        }
        async fn create_dir(&self, _: &fmnr_core::VPath) -> fmnr_core::Result<()> {
            self.nope()
        }
        async fn search(
            &self,
            _: &fmnr_core::VPath,
            _: String,
            _: usize,
        ) -> fmnr_core::Result<Vec<fmnr_core::VPath>> {
            self.nope()
        }
    }

    /// 用内存管道喂给**指定**服务端一串报文，收集所有回帧。
    async fn frames(s: &mut Server, input: &str) -> Vec<Value> {
        let mut out = Vec::new();
        s.serve(std::io::Cursor::new(input.as_bytes().to_vec()), &mut out)
            .await
            .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("回帧必须是合法 JSON"))
            .collect()
    }

    /// 用内存管道喂一串报文，收集所有回帧。
    async fn run(input: &str) -> Vec<Value> {
        let mut s = server();
        let mut out = Vec::new();
        s.serve(std::io::Cursor::new(input.as_bytes().to_vec()), &mut out)
            .await
            .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("回帧必须是合法 JSON"))
            .collect()
    }

    #[tokio::test]
    async fn initialize_returns_our_protocol_version_and_capabilities() {
        let frames = run(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#).await;
        assert_eq!(frames.len(), 1);
        let r = &frames[0]["result"];
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
        assert!(r["capabilities"]["tools"].is_object());
        assert_eq!(r["serverInfo"]["name"], "fmnr-ai");
        assert_eq!(frames[0]["id"], json!(1));
    }

    #[tokio::test]
    async fn tools_list_exposes_all_specs() {
        let frames = run(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await;
        let tools = frames[0]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), tools::specs().len());
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for want in ["list_dir", "search", "copy", "move", "delete", "open"] {
            assert!(names.contains(&want), "tools/list 少了 {want}");
        }
    }

    /// 通知（无 id）绝不能回帧——回了会让严格客户端协议错乱。
    #[tokio::test]
    async fn notification_produces_no_frame() {
        let frames = run(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
        assert!(frames.is_empty(), "通知不该产生回帧，实得 {frames:?}");
    }

    #[tokio::test]
    async fn unknown_method_reports_method_not_found() {
        let frames = run(r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#).await;
        assert_eq!(frames[0]["error"]["code"], json!(METHOD_NOT_FOUND));
    }

    #[tokio::test]
    async fn malformed_json_reports_parse_error_with_null_id() {
        let frames = run("{ this is not json").await;
        assert_eq!(frames[0]["error"]["code"], json!(PARSE_ERROR));
        assert_eq!(frames[0]["id"], Value::Null);
    }

    /// 批量请求要明确报错，而不是当成单条悄悄处理。
    #[tokio::test]
    async fn batch_request_is_rejected_explicitly() {
        let frames = run(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).await;
        assert_eq!(frames[0]["error"]["code"], json!(INVALID_REQUEST));
    }

    /// 多帧串行：一条输入一行，回帧也是一行一条，顺序对应。
    #[tokio::test]
    async fn multiple_frames_stay_line_delimited_and_ordered() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
        );
        let frames = run(input).await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], json!(1));
        assert_eq!(frames[1]["id"], json!(2));
    }

    #[tokio::test]
    async fn empty_lines_are_ignored() {
        let frames = run("\n\n\n").await;
        assert!(frames.is_empty());
    }
}
