//! fmnr-ai：MCP server（stdio JSON-RPC）+ 后续 HTTP API。
//!
//! 与 UI 共享同一套内核（`fmnr_core::Volume`），不是第二套文件系统实现。

pub mod protocol;
pub mod server;
pub mod tools;
