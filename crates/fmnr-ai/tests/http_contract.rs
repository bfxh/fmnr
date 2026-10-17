//! HTTP API 契约测试：起**真监听套接字**，用**裸 TCP 发 HTTP 报文**。
//!
//! 不用 HTTP 客户端库有两个理由：
//! 1. 不引依赖（本仓 dev-deps 保持最小）；
//! 2. 裸报文能顺带证明「服务真的在说 HTTP/1.1」——客户端库会把协议细节藏起来。
//!
//! 每个用例自建 router + 随机端口，互不干扰。

use fmnr_fs::LocalVolume;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fmnr-http-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("alpha.txt"), b"hello").unwrap();
        std::fs::write(root.join("sub/beta.log"), b"world").unwrap();
        Fixture(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// 起服务，返回 (端口, 关闭句柄)。
async fn serve(fx: &Fixture) -> u16 {
    let volume = Arc::new(LocalVolume::new(fx.0.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = fmnr_ai::http::router(volume);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

/// 发一条 HTTP 请求，返回 (状态码, body 文本)。
async fn request(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8(raw).expect("响应应是 UTF-8");

    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("拿不到状态码，原文：{text}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[tokio::test]
async fn health_says_ok() {
    let fx = Fixture::new("health");
    let port = serve(&fx).await;
    let (status, body) = request(port, "GET", "/health", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["ok"],
        true
    );
}

/// 工具清单必须与 MCP 同源——两边都读 tools::specs，数量要对得上。
#[tokio::test]
async fn tools_endpoint_matches_mcp_specs() {
    let fx = Fixture::new("tools");
    let port = serve(&fx).await;
    let (status, body) = request(port, "GET", "/tools", None).await;
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["tools"].as_array().unwrap().len(),
        fmnr_ai::tools::specs().len(),
        "HTTP 与 MCP 的工具清单必须同源"
    );
}

#[tokio::test]
async fn list_dir_over_http_returns_real_entries() {
    let fx = Fixture::new("ls");
    let port = serve(&fx).await;
    let body = format!(
        r#"{{"path": "{}"}}"#,
        fx.0.to_string_lossy().replace('\\', "\\\\")
    );
    let (status, resp) = request(port, "POST", "/tool/list_dir", Some(&body)).await;
    assert_eq!(status, 200, "响应：{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    // 根目录只有 alpha.txt 与 sub
    assert_eq!(v["count"], serde_json::json!(2), "实得 {v}");
}

/// 拷贝必须真的落盘——HTTP 这条腿也不能只看返回值。
#[tokio::test]
async fn copy_over_http_moves_bytes_on_disk() {
    let fx = Fixture::new("cp");
    let dst = fx.0.join("dest");
    std::fs::create_dir_all(&dst).unwrap();

    let port = serve(&fx).await;
    let src =
        fx.0.join("alpha.txt")
            .to_string_lossy()
            .replace('\\', "\\\\");
    let dst_s = dst.to_string_lossy().replace('\\', "\\\\");
    let body = format!(r#"{{"sources": ["{src}"], "dest_dir": "{dst_s}"}}"#);
    let (status, resp) = request(port, "POST", "/tool/copy", Some(&body)).await;
    assert_eq!(status, 200, "响应：{resp}");

    assert_eq!(std::fs::read(dst.join("alpha.txt")).unwrap(), b"hello");
    assert!(fx.0.join("alpha.txt").is_file(), "拷贝不应删源");
}

/// 参数错误 → 400（不是 500）。客户端要能靠状态码区分「我写错了」和「服务炸了」。
/// HTTP 这条腿也必须**经过注册表**：空表时要说"没有已挂载的卷"，
/// 而不是回一个空清单（那会被读成"这个目录是空的"）。
/// 这条同时证明 `router_of` 这条路是活的——如果哪天它退回持有单个卷，
/// 这里就会拿到别的回答。
#[tokio::test]
async fn empty_registry_says_so_over_http() {
    let reg = fmnr_core::volreg::VolumeRegistry::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = fmnr_ai::http::router_of(std::sync::Arc::new(reg));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (status, body) = request(port, "POST", "/tool/list_dir", Some(r#"{"path":"."}"#)).await;
    assert_eq!(
        status, 500,
        "空注册表应当是服务端错误，实得 {status} / {body}"
    );
    assert!(
        body.contains("没有已挂载的卷"),
        "文案要说清是没卷，而不是假装列出空目录：{body}"
    );
}

#[tokio::test]
async fn bad_params_are_400_not_500() {
    let fx = Fixture::new("badparam");
    let port = serve(&fx).await;
    // search 缺 query
    let body = format!(
        r#"{{"root": "{}"}}"#,
        fx.0.to_string_lossy().replace('\\', "\\\\")
    );
    let (status, resp) = request(port, "POST", "/tool/search", Some(&body)).await;
    assert_eq!(status, 400, "缺参数应是 400，实得 {status}：{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["error"].as_str().unwrap().contains("query"),
        "错误信息应点出缺 query：{v}"
    );
}

/// 未知工具 → 404。
#[tokio::test]
async fn unknown_tool_is_404() {
    let fx = Fixture::new("unknown");
    let port = serve(&fx).await;
    let (status, _) = request(port, "POST", "/tool/explode", Some("{}")).await;
    assert_eq!(status, 404);
}

/// 不存在的目录 → 工具层报错 → 400（NotFound 映射到 INVALID_PARAMS）。
#[tokio::test]
async fn listing_missing_dir_is_a_client_error() {
    let fx = Fixture::new("nope");
    let port = serve(&fx).await;
    let body = r#"{"path": "definitely-not-here"}"#;
    let (status, resp) = request(port, "POST", "/tool/list_dir", Some(body)).await;
    assert_eq!(status, 400, "响应：{resp}");
}
