//! `fmnr-ai` 可执行入口：把 stdio 接上 MCP server。
//!
//! stdout 是协议通道，日志一律走 stderr（`tracing_subscriber` 默认写 stderr）。

use fmnr_ai::server::Server;
use fmnr_fs::LocalVolume;
use std::sync::Arc;

fn main() -> std::io::Result<()> {
    // 日志去 stderr；stdout 只留给协议帧。
    // 级别固定 WARN：这是被客户端 spawn 的进程，日志多了只会淹没客户端日志面板。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();

    // 卷根：MCP 客户端没有「当前目录」概念，用启动时的工作目录做根。
    // 所有工具参数都给绝对路径，因此根只影响相对路径的解析。
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let volume = Arc::new(LocalVolume::new(root));

    let mut server = Server::new(volume);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(server.serve(stdin.lock(), stdout.lock()))
}
