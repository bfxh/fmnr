//! JSON-RPC 2.0 消息类型。
//!
//! 只实现 MCP 需要的部分：单条请求/通知/响应。
//! **批量（JSON-RPC 2.0 batch）不实现**：MCP 2025-03-26 起已移除批量语义，
//! 真实客户端不会发；收到数组会明确报 -32600 而不是静默按单条处理。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

// JSON-RPC 2.0 标准错误码
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// 一条入站消息。既可能是请求（有 id）也可能是通知（无 id）。
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
    /// 原样保留，回包时不能改动类型（数字 id 回数字，字符串 id 回字符串）。
    #[serde(default)]
    pub id: Option<Value>,
    /// `id` 字段是否**出现过**。`Option` 分不清「缺字段」与 `"id": null`，
    /// 而 JSON-RPC 2.0 里「缺 id」= 通知（不回包），"id": null 是非法但需回错。
    #[serde(skip)]
    pub has_id: bool,
}

impl Request {
    /// 从一行 JSON 解析，并修正 `has_id`。
    pub fn from_value(v: Value) -> std::result::Result<Self, RpcError> {
        let has_id = v.get("id").is_some();
        let mut req: Request =
            serde_json::from_value(v).map_err(|e| RpcError::new(INVALID_REQUEST, e.to_string()))?;
        req.has_id = has_id;
        Ok(req)
    }

    pub fn is_notification(&self) -> bool {
        !self.has_id
    }
}

/// 出站响应。`result` 与 `error` 互斥，序列化时省略 None 的那个。
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_with_id_is_not_a_notification() {
        let v = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let r = Request::from_value(v).unwrap();
        assert!(r.has_id);
        assert!(!r.is_notification());
        assert_eq!(r.method, "tools/list");
    }

    #[test]
    fn missing_id_is_a_notification() {
        let v = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        let r = Request::from_value(v).unwrap();
        assert!(!r.has_id);
        assert!(r.is_notification());
    }

    /// `"id": null` 与「没有 id」是两回事，不能被 Option 抹平。
    #[test]
    fn explicit_null_id_is_distinguished_from_absent() {
        let v = json!({"jsonrpc":"2.0","id":null,"method":"x"});
        let r = Request::from_value(v).unwrap();
        assert!(r.has_id, "显式 null id 必须被当成请求（需要回包）");
    }

    /// 回包 id 的类型必须原样保持：字符串 id 不能被转成数字。
    #[test]
    fn string_id_round_trips_as_string() {
        let v = json!({"jsonrpc":"2.0","id":"abc-1","method":"x"});
        let r = Request::from_value(v).unwrap();
        let resp = Response::ok(r.id.clone().unwrap(), json!({}));
        let s = serde_json::to_string(&resp).unwrap();
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["id"], json!("abc-1"));
    }

    #[test]
    fn error_response_omits_result_field() {
        let resp = Response::err(json!(7), RpcError::new(METHOD_NOT_FOUND, "no such method"));
        let s = serde_json::to_string(&resp).unwrap();
        let back: Value = serde_json::from_str(&s).unwrap();
        assert!(back.get("result").is_none(), "错误响应不应带 result 字段");
        assert_eq!(back["error"]["code"], json!(METHOD_NOT_FOUND));
        assert_eq!(back["id"], json!(7));
    }

    #[test]
    fn ok_response_omits_error_field() {
        let resp = Response::ok(json!(1), json!({"tools":[]}));
        let back: Value = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert!(back.get("error").is_none(), "成功响应不应带 error 字段");
    }

    #[test]
    fn malformed_request_reports_invalid_request() {
        // 缺 method
        let v = json!({"jsonrpc":"2.0","id":1});
        let e = Request::from_value(v).unwrap_err();
        assert_eq!(e.code, INVALID_REQUEST);
    }
}
