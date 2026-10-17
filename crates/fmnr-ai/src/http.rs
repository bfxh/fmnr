//! HTTP API（DESIGN §4.10 的「入向」第二条腿）。
//!
//! 与 MCP / CLI **共用同一个 `tools::dispatch_payload`**：三条入口一份行为，
//! 不存在「HTTP 能删、MCP 不能」这种分叉。
//!
//! 安全边界：这个 API 能删除文件、能启动外部程序。默认只绑 `127.0.0.1`，
//! 且**拒绝**非回环地址——除非调用方显式 `--allow-remote`。见 `bind_addr`。

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fmnr_core::Volume;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::protocol::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};
use crate::tools;

/// 共享状态：卷**注册表**。与 MCP server 持的是同一个类型，
/// 所以"哪些卷看得见"这件事两个入口不会给出不同答案。
#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<fmnr_core::volreg::VolumeRegistry>,
}

/// 建路由（单卷）。测试与"只有一个本地卷"的老调用方用这个。
pub fn router(volume: Arc<dyn Volume>) -> Router {
    let registry = fmnr_core::volreg::VolumeRegistry::new();
    registry.add(volume);
    router_of(Arc::new(registry))
}

/// 建路由（带注册表）：要让更多卷被 HTTP 客户端看到，调用方挂好再交进来。
/// 与 MCP 侧同理，这里不自己去挂归档/远程卷——那会把跨端 C 依赖拽进本 crate，
/// 取舍见 `docs/NEXT-BATCH.md` §17。
pub fn router_of(registry: Arc<fmnr_core::volreg::VolumeRegistry>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tools", get(list_tools))
        .route("/tool/{name}", post(call_tool))
        .with_state(AppState { registry })
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

/// 工具清单：与 MCP 的 `tools/list` 同源（都读 `tools::specs`）。
async fn list_tools() -> Json<Value> {
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
    Json(json!({ "tools": list }))
}

/// `POST /tool/{name}`，body 就是该工具的参数对象。
async fn call_tool(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    body: Option<Json<Value>>,
) -> Response {
    let args = body.map(|Json(v)| v).unwrap_or(json!({}));
    // 与 MCP 同一条选卷规则：按参数前缀路由，选不上回退注册表第一个。
    let Some(volume) = state.registry.route_or_default(&args) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "没有已挂载的卷：注册表是空的",
                "code": INTERNAL_ERROR
            })),
        )
            .into_response();
    };
    match tools::dispatch_payload(volume, &name, args).await {
        Ok(payload) => (StatusCode::OK, Json(payload)).into_response(),
        Err(e) => {
            // JSON-RPC 错误码 → HTTP 状态码。别一律 500：
            // 客户端要靠状态码区分「我参数写错了」和「服务炸了」。
            let status = match e.code {
                INVALID_PARAMS => StatusCode::BAD_REQUEST,
                METHOD_NOT_FOUND => StatusCode::NOT_FOUND,
                INTERNAL_ERROR => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({ "error": e.message, "code": e.code }))).into_response()
        }
    }
}

/// 决定监听地址。**非回环地址必须显式放行**。
///
/// 理由：这个 HTTP 面能删除文件、能拉起外部程序。默认只绑回环，
/// 把「谁能调」限死在同机。要对外提供就得由用户自己 `allow_remote = true`，
/// 让「暴露到网络」变成一个必须写下来的决定，而不是默认行为。
pub fn bind_addr(host: &str, port: u16, allow_remote: bool) -> Result<String, String> {
    let loopback = host == "127.0.0.1" || host == "::1" || host == "localhost" || host == "[::1]";
    if !loopback && !allow_remote {
        return Err(format!(
            "拒绝绑定非回环地址 {host}：该 API 可删除文件、可启动外部程序。\
             确认要对外暴露请显式加 --allow-remote。"
        ));
    }
    // IPv6 字面量要加方括号才能拼进 host:port
    let h = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("{h}:{port}"))
}

/// 起服务并阻塞（单卷）。`addr` 形如 `127.0.0.1:8787`。
pub async fn run(volume: Arc<dyn Volume>, addr: &str) -> std::io::Result<()> {
    let registry = fmnr_core::volreg::VolumeRegistry::new();
    registry.add(volume);
    run_of(Arc::new(registry), addr).await
}

/// 起服务并阻塞（带注册表）。
pub async fn run_of(
    registry: Arc<fmnr_core::volreg::VolumeRegistry>,
    addr: &str,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router_of(registry)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_addresses_are_allowed_by_default() {
        assert!(bind_addr("127.0.0.1", 8787, false).is_ok());
        assert!(bind_addr("localhost", 8787, false).is_ok());
        assert!(bind_addr("::1", 8787, false).is_ok());
    }

    /// 这条是安全边界本身：不显式放行就绝不能绑到对外地址。
    #[test]
    fn non_loopback_is_refused_without_opt_in() {
        let e = bind_addr("0.0.0.0", 8787, false).unwrap_err();
        assert!(e.contains("--allow-remote"), "错误要告诉用户怎么放行：{e}");
        assert!(bind_addr("192.168.1.10", 8787, false).is_err());
        // 显式放行后才行
        assert!(bind_addr("0.0.0.0", 8787, true).is_ok());
    }

    #[test]
    fn ipv6_literal_gets_brackets() {
        assert_eq!(bind_addr("::1", 8787, false).unwrap(), "[::1]:8787");
        assert_eq!(bind_addr("127.0.0.1", 80, false).unwrap(), "127.0.0.1:80");
    }
}
