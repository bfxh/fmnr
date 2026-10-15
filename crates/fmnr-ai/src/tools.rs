//! MCP 工具注册表：工具名 → 描述 + JSON Schema + 分发。
//!
//! 工具全部**复用内核**（`fmnr_core::Volume`），不另开一条文件系统通路——
//! 否则 MCP 客户端看到的行为会和 UI 不一致（DESIGN §4.10）。
//!
//! 每个工具的参数校验都走 `serde_json::from_value` 到强类型 struct，
//! 缺字段/类型错会得到 -32602 且带**具体哪个字段**，而不是笼统的 "invalid params"。

use fmnr_core::{ConflictPolicy, ListOpts, SortOrder, VPath, Volume};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::protocol::{RpcError, INVALID_PARAMS};

/// 工具的静态描述，用于 `tools/list`。
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema（MCP 要求 inputSchema 是 object 类型）
    pub input_schema: Value,
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "list_volume",
            description: "列出已挂载的卷（本地盘等）。",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "list_dir",
            description: "列出目录内容。path 为绝对路径；省略则列当前卷根。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "绝对目录路径"},
                    "show_hidden": {"type": "boolean", "default": false},
                    "sort": {"type": "string", "enum": ["name", "size", "modified", "type"], "default": "name"}
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "search",
            description: "在当前目录子树内按文件名子串搜索。返回前 limit 条。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "root": {"type": "string", "description": "搜索起点目录（绝对路径）"},
                    "query": {"type": "string", "description": "文件名子串，大小写不敏感"},
                    "limit": {"type": "integer", "minimum": 1, "default": 200}
                },
                "required": ["root", "query"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "copy",
            description: "拷贝一组条目到目标目录。conflict 决定同名时怎么办。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sources": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                    "dest_dir": {"type": "string"},
                    "conflict": {"type": "string", "enum": ["overwrite", "skip", "rename"], "default": "skip"}
                },
                "required": ["sources", "dest_dir"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "move",
            description: "移动一组条目到目标目录。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sources": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                    "dest_dir": {"type": "string"},
                    "conflict": {"type": "string", "enum": ["overwrite", "skip", "rename"], "default": "skip"}
                },
                "required": ["sources", "dest_dir"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "delete",
            description: "删除条目。本地卷走回收站，可恢复。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "targets": {"type": "array", "items": {"type": "string"}, "minItems": 1}
                },
                "required": ["targets"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "open",
            description: "按打开决策表打开文件：可预览类型走内置预览（返回 route=preview），否则启动外部程序并挂状态角标。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "force_external": {"type": "boolean", "default": false,
                        "description": "true = 即使是可预览类型也用外部程序打开"},
                    "program": {"type": "string",
                        "description": "指定外部程序；省略则用系统默认"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "external_opens",
            description: "当前被外部程序打开的文件（角标状态）。进程退出后自动消失。",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
    ]
}

// ---- 参数强类型 ----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListDirArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    show_hidden: bool,
    #[serde(default)]
    sort: SortArg,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SortArg {
    #[default]
    Name,
    Size,
    Modified,
    Type,
}

impl SortArg {
    fn to_core(&self) -> SortOrder {
        match self {
            SortArg::Name => SortOrder::Name,
            SortArg::Size => SortOrder::Size,
            SortArg::Modified => SortOrder::Modified,
            SortArg::Type => SortOrder::Type,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    root: String,
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    200
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferArgs {
    sources: Vec<String>,
    dest_dir: String,
    #[serde(default)]
    conflict: ConflictArg,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ConflictArg {
    Overwrite,
    #[default]
    Skip,
    Rename,
}

impl ConflictArg {
    fn to_core(&self) -> ConflictPolicy {
        match self {
            ConflictArg::Overwrite => ConflictPolicy::Overwrite,
            ConflictArg::Skip => ConflictPolicy::Skip,
            ConflictArg::Rename => ConflictPolicy::Rename,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    targets: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenArgs {
    path: String,
    #[serde(default)]
    force_external: bool,
    #[serde(default)]
    program: Option<String>,
}

/// 把参数解析失败翻译成 -32602，并带上 serde 的字段级原因。
fn parse<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(|e| RpcError::new(INVALID_PARAMS, e.to_string()))
}

/// 分发表。返回 `Result<Value, RpcError>`；`Value` 是 MCP 的 `content` 数组。
pub async fn dispatch(volume: Arc<dyn Volume>, name: &str, args: Value) -> Result<Value, RpcError> {
    match name {
        "list_volume" => {
            let _: Value = parse(args)?;
            let id = volume.id();
            Ok(text_result(json!({
                "volumes": [{
                    "id": id.0,
                    "kind": format!("{:?}", volume.kind()),
                    "writable": volume.writable(),
                }]
            })))
        }

        "list_dir" => {
            let a: ListDirArgs = parse(args)?;
            let path = a.path.unwrap_or_default();
            let opts = ListOpts {
                show_hidden: a.show_hidden,
                sort: a.sort.to_core(),
            };
            let entries = volume
                .list(&VPath(path.clone().into()), &opts)
                .await
                .map_err(to_rpc)?;
            let items: Vec<Value> = entries
                .iter()
                .map(|e| {
                    json!({
                        "name": e.name,
                        "is_dir": e.is_dir,
                        "size": e.size,
                        "modified_ms": e.modified.and_then(|t| t
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_millis() as u64)),
                    })
                })
                .collect();
            Ok(text_result(
                json!({ "path": path, "count": items.len(), "entries": items }),
            ))
        }

        "search" => {
            let a: SearchArgs = parse(args)?;
            if a.query.trim().is_empty() {
                return Err(RpcError::new(INVALID_PARAMS, "query 不能为空"));
            }
            let root = std::path::PathBuf::from(&a.root);
            let limit = a.limit;
            // 走内核的 Volume 搜索面，与 UI 同一通路。
            let hits = volume
                .search(&VPath(root.clone()), a.query.clone(), limit)
                .await
                .map_err(to_rpc)?;
            Ok(text_result(json!({
                "root": a.root,
                "query": a.query,
                "count": hits.len(),
                "hits": hits,
            })))
        }

        "copy" | "move" => {
            let a: TransferArgs = parse(args)?;
            if a.sources.is_empty() {
                return Err(RpcError::new(INVALID_PARAMS, "sources 不能为空"));
            }
            let sources: Vec<VPath> = a.sources.iter().map(|s| VPath(s.into())).collect();
            let dest = VPath(a.dest_dir.clone().into());
            let progress = Arc::new(fmnr_core::Progress::new());
            let report = if name == "copy" {
                volume
                    .copy(&sources, &dest, a.conflict.to_core(), progress)
                    .await
            } else {
                volume
                    .move_entries(&sources, &dest, a.conflict.to_core(), progress)
                    .await
            }
            .map_err(to_rpc)?;
            Ok(text_result(json!({
                "done": report.done,
                "skipped": report.skipped,
                "aborted": report.aborted,
                "bytes": report.bytes,
            })))
        }

        "delete" => {
            let a: DeleteArgs = parse(args)?;
            if a.targets.is_empty() {
                return Err(RpcError::new(INVALID_PARAMS, "targets 不能为空"));
            }
            let targets: Vec<VPath> = a.targets.iter().map(|s| VPath(s.into())).collect();
            let progress = Arc::new(fmnr_core::Progress::new());
            let report = volume.delete(&targets, progress).await.map_err(to_rpc)?;
            Ok(text_result(json!({
                "done": report.done,
                "skipped": report.skipped,
                "aborted": report.aborted,
            })))
        }

        "open" => {
            let a: OpenArgs = parse(args)?;
            let path = std::path::PathBuf::from(&a.path);
            let route = fmnr_core::open::route_open(&path, a.force_external);
            match route {
                fmnr_core::open::OpenRoute::Preview => Ok(text_result(json!({
                    "route": "preview",
                    "path": a.path,
                    "note": "该类型走内置预览；本服务不渲染，请由 UI 呈现",
                }))),
                fmnr_core::open::OpenRoute::External(default_prog) => {
                    let program = a.program.unwrap_or(default_prog);
                    let open = volume
                        .spawn_external_open(&VPath(path.clone()), &program)
                        .await
                        .map_err(to_rpc)?;
                    volume
                        .external_opens()
                        .insert(path.display().to_string(), open.clone());
                    Ok(text_result(json!({
                        "route": "external",
                        "path": a.path,
                        "program": open.program,
                        "started_at_ms": open.started_at_ms,
                    })))
                }
            }
        }

        "external_opens" => {
            let _: Value = parse(args)?;
            let items: Vec<Value> = volume
                .external_opens()
                .iter()
                .into_iter()
                .map(|(file, o)| {
                    json!({
                        "file": file,
                        "program": o.program,
                        "started_at_ms": o.started_at_ms,
                    })
                })
                .collect();
            Ok(text_result(json!({ "count": items.len(), "opens": items })))
        }

        other => Err(RpcError::new(
            crate::protocol::METHOD_NOT_FOUND,
            format!("未知工具：{other}"),
        )),
    }
}

/// 把内核错误映射成 JSON-RPC 错误码。
fn to_rpc(e: fmnr_core::Error) -> RpcError {
    use fmnr_core::Error as E;
    let code = match &e {
        E::NotFound(_) => INVALID_PARAMS,
        E::Unsupported(_) => crate::protocol::INTERNAL_ERROR,
        E::Io(_) | E::Other(_) => crate::protocol::INTERNAL_ERROR,
    };
    RpcError::new(code, e.to_string())
}

/// MCP 工具返回体：`content` 是块数组。这里统一给一个 text 块，内容为 JSON 字符串。
fn text_result(payload: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
        }],
        "isError": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fmnr_fs::LocalVolume;

    fn vol() -> Arc<dyn Volume> {
        Arc::new(LocalVolume::new(std::env::temp_dir()))
    }

    #[test]
    fn every_spec_has_object_schema() {
        for s in specs() {
            assert_eq!(
                s.input_schema["type"], "object",
                "{} 的 inputSchema 必须是 object（MCP 要求）",
                s.name
            );
            assert!(!s.description.is_empty(), "{} 缺描述", s.name);
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<_> = specs().iter().map(|s| s.name).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "工具名重复");
    }

    /// 未知工具必须报 METHOD_NOT_FOUND，不能悄悄返回空结果。
    #[tokio::test]
    async fn unknown_tool_reports_method_not_found() {
        let e = dispatch(vol(), "no_such_tool", json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.code, crate::protocol::METHOD_NOT_FOUND);
    }

    /// 缺必填字段要报 -32602 且能指出是哪个字段。
    #[tokio::test]
    async fn missing_required_field_names_the_field() {
        let e = dispatch(vol(), "search", json!({"root": "C:\\"}))
            .await
            .unwrap_err();
        assert_eq!(e.code, INVALID_PARAMS);
        assert!(
            e.message.contains("query"),
            "错误信息应指出缺的是 query：{}",
            e.message
        );
    }

    /// 多余字段要拒绝：静默忽略拼错的参数会让客户端以为生效了。
    #[tokio::test]
    async fn unknown_field_is_rejected() {
        let e = dispatch(vol(), "list_dir", json!({"path": ".", "sortt": "name"}))
            .await
            .unwrap_err();
        assert_eq!(e.code, INVALID_PARAMS);
    }

    /// 枚举值拼错要被抓住，而不是回落到默认。
    #[tokio::test]
    async fn bad_enum_value_is_rejected() {
        let e = dispatch(vol(), "list_dir", json!({"sort": "sizee"}))
            .await
            .unwrap_err();
        assert_eq!(e.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn empty_sources_rejected() {
        let e = dispatch(vol(), "copy", json!({"sources": [], "dest_dir": "C:\\"}))
            .await
            .unwrap_err();
        assert_eq!(e.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn empty_search_query_rejected() {
        let e = dispatch(vol(), "search", json!({"root": ".", "query": "   "}))
            .await
            .unwrap_err();
        assert_eq!(e.code, INVALID_PARAMS);
    }
}
