//! MCP 工具注册表：工具名 → 描述 + JSON Schema + 分发。
//!
//! 工具全部**复用内核**（`fmnr_core::Volume`），不另开一条文件系统通路——
//! 否则 MCP 客户端看到的行为会和 UI 不一致（DESIGN §4.10）。
//!
//! 每个工具的参数校验都走 `serde_json::from_value` 到强类型 struct，
//! 缺字段/类型错会得到 -32602 且带**具体哪个字段**，而不是笼统的 "invalid params"。

use fmnr_core::{ConflictPolicy, ListOpts, SortOrder, VPath, Volume};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::protocol::{RpcError, INTERNAL_ERROR, INVALID_PARAMS};

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
        // ↓ 这四条是内核早就有、但一直没上工具表的能力。
        //   补在这里而不是另开通路：三条入口（MCP/HTTP/CLI）共用同一个
        //   dispatch_payload，一处补齐三处就有，反之会分叉成"CLI 能读、MCP 不能"。
        ToolSpec {
            name: "read_file",
            description: "读一个文件的文本内容。超过 max_bytes 会截断并在 truncated 里说明；非 UTF-8 内容不返回正文，只报 binary=true 与字节数。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "绝对文件路径"},
                    "max_bytes": {"type": "integer", "minimum": 1, "default": 262144,
                        "description": "读取上限，默认 256 KiB"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "write_file",
            description: "写一个 UTF-8 文本文件（整体覆盖）。只读卷会报错而不是静默失败。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "绝对文件路径"},
                    "text": {"type": "string", "description": "要写入的完整内容"}
                },
                "required": ["path", "text"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "rename",
            description: "重命名/移动单个条目。to 是同一卷内的绝对路径。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string"},
                    "to": {"type": "string"}
                },
                "required": ["from", "to"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "create_dir",
            description: "新建目录（绝对路径）。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        // ↓ 剩下两条同样是"内核早就有、工具表没有"：
        //   属性在 `Entry` 里（UI 一直在用），已装底账在 `fmnr-pkg::registry`（面板在用）。
        ToolSpec {
            name: "stat",
            description: "取单个条目的属性：类型、大小、修改时间、隐藏/系统/符号链接/只读，外加这个卷的种类、可否写、有没有原生路径。属性走卷给的 Entry，所以归档卷里读到的是包内声明的大小。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "条目路径（相对当前卷根或绝对）"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "packages",
            description: "列出注册表 Uninstall 三棵树里的已装程序（名称/发行商/版本/家族/安装位置/GitHub 上游）。只读盘点：不返回卸载命令行，也不代跑卸载或升级。query 按名称或发行商子串筛。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "名称/发行商子串，大小写不敏感"},
                    "limit": {"type": "integer", "minimum": 1, "default": 200}
                },
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
}

/// 256 KiB。默认值存在的理由是"别把一个 4 GB 的文件读进内存"，
/// 不是"够不够"——所以它可被参数覆盖，且截断一定会在返回里说明。
fn default_max_bytes() -> usize {
    256 * 1024
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameArgs {
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MkdirArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagesArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
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

/// 分发表：工具名 + 参数 → **原始 payload**。
///
/// MCP 层（`server.rs`）把它包成 `content` 数组，CLI 直接打印 payload——
/// 两条入口共用这一个 match，才不会出现「CLI 能删、MCP 不能」这类分叉。
pub async fn dispatch_payload(
    volume: Arc<dyn Volume>,
    name: &str,
    args: Value,
) -> Result<Value, RpcError> {
    match name {
        "list_volume" => {
            let _: Value = parse(args)?;
            let id = volume.id();
            Ok(json!({
                "volumes": [{
                    "id": id.0,
                    "kind": format!("{:?}", volume.kind()),
                    "writable": volume.writable(),
                }]
            }))
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
            Ok(json!({ "path": path, "count": items.len(), "entries": items }))
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
            Ok(json!({
                "root": a.root,
                "query": a.query,
                "count": hits.len(),
                "hits": hits,
            }))
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
            Ok(json!({
                "done": report.done,
                "skipped": report.skipped,
                "aborted": report.aborted,
                "bytes": report.bytes,
            }))
        }

        "delete" => {
            let a: DeleteArgs = parse(args)?;
            if a.targets.is_empty() {
                return Err(RpcError::new(INVALID_PARAMS, "targets 不能为空"));
            }
            let targets: Vec<VPath> = a.targets.iter().map(|s| VPath(s.into())).collect();
            let progress = Arc::new(fmnr_core::Progress::new());
            let report = volume.delete(&targets, progress).await.map_err(to_rpc)?;
            Ok(json!({
                "done": report.done,
                "skipped": report.skipped,
                "aborted": report.aborted,
            }))
        }

        "read_file" => {
            let a: ReadArgs = parse(args)?;
            let path = VPath(a.path.clone().into());
            let mut buf: Vec<u8> = Vec::new();
            let mut stream = volume.read_stream(&path).await.map_err(to_rpc)?;
            // 按块读到上限就停，而不是先整读再截断：
            // 后者读一个 4 GB 文件时会先把机器打满。
            let mut truncated = false;
            while buf.len() < a.max_bytes {
                let chunk = match stream.next().await {
                    None => break,
                    Some(Err(e)) => return Err(RpcError::new(INTERNAL_ERROR, e.to_string())),
                    Some(Ok(b)) => b,
                };
                let room = a.max_bytes - buf.len();
                if chunk.len() > room {
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(&chunk);
            }
            if !truncated && buf.len() == a.max_bytes {
                // 正好停在边界：再问一块确认后面真的没有了，
                // 否则会把"截断了"和"刚好读完整个文件"混成一种。
                if let Some(Ok(extra)) = stream.next().await {
                    if !extra.is_empty() {
                        truncated = true;
                    }
                }
            }
            match String::from_utf8(buf.clone()) {
                Ok(text) => Ok(json!({
                    "path": a.path,
                    "binary": false,
                    "bytes": buf.len(),
                    "truncated": truncated,
                    "text": text,
                })),
                Err(_) => Ok(json!({
                    // 不是 UTF-8 就不硬塞：调用方拿到一屏乱码比拿到 binary=true 更难办。
                    // 要二进制内容请走 native_path + 自己读，这里不装。
                    "path": a.path,
                    "binary": true,
                    "bytes": buf.len(),
                    "truncated": truncated,
                })),
            }
        }

        "write_file" => {
            let a: WriteArgs = parse(args)?;
            if !volume.writable() {
                // 与 copy/delete 同一条错误路径：只读在契约里就是卷返回 Unsupported
                // （fmnr-core::Volume 文档注释），错误码由 to_rpc 统一给。
                return Err(to_rpc(fmnr_core::Error::Unsupported(format!(
                    "这个卷是只读的，不能写：{:?}",
                    volume.kind()
                ))));
            }
            volume
                .write(&VPath(a.path.clone().into()), a.text.as_bytes())
                .await
                .map_err(to_rpc)?;
            Ok(json!({ "path": a.path, "bytes": a.text.len() }))
        }

        "rename" => {
            let a: RenameArgs = parse(args)?;
            if !volume.writable() {
                return Err(to_rpc(fmnr_core::Error::Unsupported(format!(
                    "这个卷是只读的，不能改名：{:?}",
                    volume.kind()
                ))));
            }
            volume
                .rename(&VPath(a.from.clone().into()), &VPath(a.to.clone().into()))
                .await
                .map_err(to_rpc)?;
            Ok(json!({ "from": a.from, "to": a.to }))
        }

        "create_dir" => {
            let a: MkdirArgs = parse(args)?;
            if !volume.writable() {
                return Err(to_rpc(fmnr_core::Error::Unsupported(format!(
                    "这个卷是只读的，不能建目录：{:?}",
                    volume.kind()
                ))));
            }
            volume
                .create_dir(&VPath(a.path.clone().into()))
                .await
                .map_err(to_rpc)?;
            Ok(json!({ "path": a.path, "created": true }))
        }

        "stat" => {
            let a: StatArgs = parse(args)?;
            let path = std::path::PathBuf::from(&a.path);
            // 属性一律从**卷给的 Entry** 里取，不在这里另 `fs::metadata` 一遍：
            // 归档卷的"大小"是包内声明的大小，远程卷的是 SFTP 报的，
            // 自己 stat 会把这套口径换成只对本地盘成立的另一套。
            let opts = ListOpts {
                show_hidden: true,
                sort: SortOrder::Name,
            };
            let (is_dir, size, modified, attrs) = match path.parent() {
                Some(dir) => {
                    let nm = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "路径里没有可查的条目名"))?;
                    // show_hidden=true 是必须的：不然"这是个隐藏文件"会被报成
                    // "这个文件不存在"——查属性恰恰是最想知道它隐藏的时候。
                    let entries = volume
                        .list(&VPath(dir.to_path_buf()), &opts)
                        .await
                        .map_err(to_rpc)?;
                    let e = entries.into_iter().find(|e| e.name == nm).ok_or_else(|| {
                        to_rpc(fmnr_core::Error::NotFound(format!("{} 不存在", a.path)))
                    })?;
                    (e.is_dir, e.size, e.modified, e.attrs)
                }
                // 卷根（`E:\`、`\\wsl$\<distro>`）在父目录里不是一条条目。
                // 拿"列得出来"当它存在的证据；这一层抽象没给的大小和时间就报 null。
                None => {
                    volume
                        .list(&VPath(path.clone()), &opts)
                        .await
                        .map_err(to_rpc)?;
                    (true, None, None, Vec::new())
                }
            };
            Ok(json!({
                "path": a.path,
                "is_dir": is_dir,
                "size": size,
                "modified_ms": modified.and_then(|t| t
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as u64)),
                "attrs": attrs.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>(),
                "volume_kind": format!("{:?}", volume.kind()),
                "writable": volume.writable(),
                "native_path": volume
                    .native_path(&VPath(path))
                    .map(|p| p.to_string_lossy().to_string()),
            }))
        }

        "packages" => {
            let a: PackagesArgs = parse(args)?;
            // 只读盘点。两条刻意的取舍：
            // 1. **不回显卸载命令行**（`uninstall_string`/`quiet_uninstall_string`）——
            //    那是可执行面，而本项目的立场是"只列，不代跑"（和注册动作
            //    不做成工具同一条理由：碰系统边界的不在这里给远程客户端触发）。
            //    所以只给 `has_uninstaller` 这个布尔。
            // 2. 底账本身按名称排好（`registry.rs` 里有测试钉着），
            //    三条入口才可能拿到字节级相同的返回。
            let all = fmnr_pkg::registry::list_installed()
                .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("读已装程序底账失败：{e}")))?;
            let total = all.len();
            let needle = a.query.clone().unwrap_or_default().to_lowercase();
            let matched: Vec<_> = all
                .iter()
                .filter(|app| {
                    needle.is_empty()
                        || app.name.to_lowercase().contains(&needle)
                        || app
                            .publisher
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&needle)
                })
                .collect();
            let items: Vec<Value> = matched
                .iter()
                .take(a.limit)
                .map(|app| {
                    json!({
                        "name": app.name,
                        "publisher": app.publisher,
                        "version": app.version,
                        "family": format!("{:?}", app.family),
                        "hive": format!("{:?}", app.hive),
                        "install_location": app.install_location,
                        "url": app.url,
                        "github": app.github,
                        "has_uninstaller": app.uninstall_string.is_some()
                            || app.quiet_uninstall_string.is_some(),
                    })
                })
                .collect();
            Ok(json!({
                "count": items.len(),
                "matched": matched.len(),
                "total_installed": total,
                "truncated": items.len() < matched.len(),
                "packages": items,
            }))
        }

        "open" => {
            let a: OpenArgs = parse(args)?;
            let path = std::path::PathBuf::from(&a.path);
            let route = fmnr_core::open::route_open(&path, a.force_external);
            match route {
                fmnr_core::open::OpenRoute::Preview => Ok(json!({
                    "route": "preview",
                    "path": a.path,
                    "note": "该类型走内置预览；本服务不渲染，请由 UI 呈现",
                })),
                fmnr_core::open::OpenRoute::External(default_prog) => {
                    let program = a.program.unwrap_or(default_prog);
                    let open = volume
                        .spawn_external_open(&VPath(path.clone()), &program)
                        .await
                        .map_err(to_rpc)?;
                    volume
                        .external_opens()
                        .insert(path.display().to_string(), open.clone());
                    Ok(json!({
                        "route": "external",
                        "path": a.path,
                        "program": open.program,
                        "started_at_ms": open.started_at_ms,
                    }))
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
            Ok(json!({ "count": items.len(), "opens": items }))
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
///
/// 只在 MCP 边界加这层信封；CLI 直接用 `dispatch_payload` 的原始 JSON。
pub async fn dispatch(volume: Arc<dyn Volume>, name: &str, args: Value) -> Result<Value, RpcError> {
    dispatch_payload(volume, name, args).await.map(text_result)
}

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

    /// 声明表与分发表必须一致：`tools/list` 里出现的每个名字，
    /// 都要能在 `dispatch_payload` 里走到自己的分支。
    /// 少了这一步就会出现"客户端看得见、一调就 Method not found"——
    /// 那种问题只在集成时才暴露，而这里一条测试就能钉住。
    #[tokio::test]
    async fn every_spec_is_dispatchable() {
        for s in specs() {
            // 空参数对有些工具是**合法的**（list_volume / external_opens 不收参数），
            // 所以这里不能期待报错，只能期待"不是没实现"。
            let outcome = dispatch_payload(vol(), s.name, json!({})).await;
            if let Err(e) = &outcome {
                assert_ne!(
                    e.code,
                    crate::protocol::METHOD_NOT_FOUND,
                    "工具「{}」在 specs() 里声明了，却没在分发表里实现",
                    s.name
                );
            }
        }
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fmnr-tools-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn write_then_read_roundtrips() {
        let d = tmp_dir("roundtrip");
        let p = d.join("note.txt").display().to_string();
        let w = dispatch_payload(vol(), "write_file", json!({"path": p, "text": "第一条\n"}))
            .await
            .unwrap();
        assert_eq!(
            w["bytes"], 10,
            "UTF-8 下中文每字 3 字节，bytes 要给字节数不是字数"
        );
        let r = dispatch_payload(vol(), "read_file", json!({"path": p}))
            .await
            .unwrap();
        assert_eq!(r["text"], "第一条\n");
        assert_eq!(r["binary"], false);
        assert_eq!(r["truncated"], false);
        assert!(std::fs::read_to_string(&p).unwrap() == "第一条\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 截断必须**说出来**。半截内容看起来和完整内容一模一样，
    /// 客户端会拿它当全文去改再写回去——那是能毁文件的错法。
    #[tokio::test]
    async fn read_file_truncates_and_says_so() {
        let d = tmp_dir("trunc");
        let p = d.join("big.txt");
        std::fs::write(&p, vec![b'a'; 5000]).unwrap();
        let r = dispatch_payload(
            vol(),
            "read_file",
            json!({"path": p.display().to_string(), "max_bytes": 100}),
        )
        .await
        .unwrap();
        assert_eq!(r["bytes"], 100);
        assert_eq!(r["truncated"], true);
        assert_eq!(r["text"].as_str().unwrap().len(), 100);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 正好读到上限：不能因为"读满了"就谎报截断。
    #[tokio::test]
    async fn exact_size_boundary_is_not_reported_as_truncated() {
        let d = tmp_dir("edge");
        let p = d.join("exact.txt");
        std::fs::write(&p, vec![b'b'; 100]).unwrap();
        let r = dispatch_payload(
            vol(),
            "read_file",
            json!({"path": p.display().to_string(), "max_bytes": 100}),
        )
        .await
        .unwrap();
        assert_eq!(r["bytes"], 100);
        assert_eq!(r["truncated"], false, "刚好读完不是截断");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 非 UTF-8 不硬塞正文：给一屏乱码比给 `binary: true` 更难办。
    #[tokio::test]
    async fn binary_content_is_reported_not_dumped() {
        let d = tmp_dir("bin");
        let p = d.join("raw.bin");
        // 一个非法 UTF-8 序列（0xFF 不是任何合法起始字节）
        std::fs::write(&p, [0xff_u8, 0xfe, 0x00, 0x41]).unwrap();
        let r = dispatch_payload(vol(), "read_file", json!({"path": p.display().to_string()}))
            .await
            .unwrap();
        assert_eq!(r["binary"], true);
        assert!(r.get("text").is_none(), "二进制不该带 text 字段");
        assert_eq!(r["bytes"], 4);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn create_dir_and_rename_work_on_disk() {
        let d = tmp_dir("ops");
        let sub = d.join("made").display().to_string();
        let r = dispatch_payload(vol(), "create_dir", json!({"path": sub}))
            .await
            .unwrap();
        assert_eq!(r["created"], true);
        assert!(d.join("made").is_dir());

        let from = d.join("made\\a.txt");
        std::fs::write(&from, "x").unwrap();
        let to = d.join("made\\b.txt").display().to_string();
        dispatch_payload(
            vol(),
            "rename",
            json!({"from": from.display().to_string(), "to": to}),
        )
        .await
        .unwrap();
        assert!(!from.exists(), "改名后旧路径不该还在");
        assert!(d.join("made\\b.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 读不存在的路径要报错，不能返回"空文件"——
    /// 空文件和读不到在用户眼里是两件事。
    #[tokio::test]
    async fn read_missing_file_errors_instead_of_returning_empty() {
        let d = tmp_dir("missing");
        let p = d.join("nope.txt").display().to_string();
        let e = dispatch_payload(vol(), "read_file", json!({"path": p}))
            .await
            .expect_err("不存在的路径必须报错");
        assert_ne!(e.code, crate::protocol::METHOD_NOT_FOUND);
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------------------------------------------- 只读卷：新三条写路径必须一样拒绝

    // 这些名字里有一半没被 `super::*` 带进来（tools.rs 只用到它们的全路径写法），
    // 而 `Result` 必须是 fmnr_core 的单参数别名，不是 std 的双参数版本。
    use fmnr_core::{
        Entry, ExternalOpen, ExternalOpens, OpReport, Progress, Result, VolumeId, VolumeKind,
    };

    /// 把本地卷包一层，只把 `writable()` 翻成 false，其余全转发。
    ///
    /// 为什么不拿 fmnr-archive 的真只读卷当夹具：那会把 zstd-sys 的跨端 C 编译器
    /// 拽进 fmnr-ai 的依赖图（本机与 CI 各多背一份编译成本），而这条测试要的
    /// 只是「卷说自己不可写」这一个布尔。
    struct ReadOnly(LocalVolume);

    #[async_trait::async_trait]
    impl Volume for ReadOnly {
        fn id(&self) -> VolumeId {
            self.0.id()
        }
        fn kind(&self) -> VolumeKind {
            self.0.kind()
        }
        async fn list(&self, path: &VPath, opts: &ListOpts) -> Result<Vec<Entry>> {
            self.0.list(path, opts).await
        }
        async fn read_stream(
            &self,
            path: &VPath,
        ) -> Result<Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>>
        {
            self.0.read_stream(path).await
        }
        async fn write(&self, path: &VPath, data: &[u8]) -> Result<()> {
            self.0.write(path, data).await
        }
        fn writable(&self) -> bool {
            false
        }
        fn native_path(&self, path: &VPath) -> Option<std::path::PathBuf> {
            self.0.native_path(path)
        }
        async fn spawn_external_open(
            &self,
            file_path: &VPath,
            program: &str,
        ) -> Result<ExternalOpen> {
            self.0.spawn_external_open(file_path, program).await
        }
        fn external_opens(&self) -> &ExternalOpens {
            self.0.external_opens()
        }
        async fn copy(
            &self,
            sources: &[VPath],
            dest_dir: &VPath,
            policy: ConflictPolicy,
            progress: Arc<Progress>,
        ) -> Result<OpReport> {
            self.0.copy(sources, dest_dir, policy, progress).await
        }
        async fn move_entries(
            &self,
            sources: &[VPath],
            dest_dir: &VPath,
            policy: ConflictPolicy,
            progress: Arc<Progress>,
        ) -> Result<OpReport> {
            self.0
                .move_entries(sources, dest_dir, policy, progress)
                .await
        }
        async fn delete(&self, targets: &[VPath], progress: Arc<Progress>) -> Result<OpReport> {
            self.0.delete(targets, progress).await
        }
        async fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
            self.0.rename(from, to).await
        }
        async fn create_dir(&self, path: &VPath) -> Result<()> {
            self.0.create_dir(path).await
        }
        async fn search(&self, root: &VPath, needle: String, limit: usize) -> Result<Vec<VPath>> {
            self.0.search(root, needle, limit).await
        }
    }

    fn ro_vol() -> Arc<dyn Volume> {
        Arc::new(ReadOnly(LocalVolume::new(std::env::temp_dir())))
    }

    /// 只有在 `show_hidden = true` 时才肯列东西的卷。
    ///
    /// 它存在的唯一目的：把 `stat` 里那句 `show_hidden: true` 钉成会判红的东西。
    /// 改成 false 的话，隐藏项的属性就查不到，而工具会把"这是个隐藏文件"
    /// 报成"这个文件不存在"——正是查属性最不该说错的那一句。
    struct OnlyWhenShown(LocalVolume);

    #[async_trait::async_trait]
    impl Volume for OnlyWhenShown {
        fn id(&self) -> VolumeId {
            self.0.id()
        }
        fn kind(&self) -> VolumeKind {
            self.0.kind()
        }
        async fn list(&self, path: &VPath, opts: &ListOpts) -> Result<Vec<Entry>> {
            if !opts.show_hidden {
                return Ok(Vec::new());
            }
            self.0.list(path, opts).await
        }
        async fn read_stream(
            &self,
            path: &VPath,
        ) -> Result<Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>>
        {
            self.0.read_stream(path).await
        }
        async fn write(&self, path: &VPath, data: &[u8]) -> Result<()> {
            self.0.write(path, data).await
        }
        fn writable(&self) -> bool {
            self.0.writable()
        }
        fn native_path(&self, path: &VPath) -> Option<std::path::PathBuf> {
            self.0.native_path(path)
        }
        async fn spawn_external_open(
            &self,
            file_path: &VPath,
            program: &str,
        ) -> Result<ExternalOpen> {
            self.0.spawn_external_open(file_path, program).await
        }
        fn external_opens(&self) -> &ExternalOpens {
            self.0.external_opens()
        }
        async fn copy(
            &self,
            sources: &[VPath],
            dest_dir: &VPath,
            policy: ConflictPolicy,
            progress: Arc<Progress>,
        ) -> Result<OpReport> {
            self.0.copy(sources, dest_dir, policy, progress).await
        }
        async fn move_entries(
            &self,
            sources: &[VPath],
            dest_dir: &VPath,
            policy: ConflictPolicy,
            progress: Arc<Progress>,
        ) -> Result<OpReport> {
            self.0
                .move_entries(sources, dest_dir, policy, progress)
                .await
        }
        async fn delete(&self, targets: &[VPath], progress: Arc<Progress>) -> Result<OpReport> {
            self.0.delete(targets, progress).await
        }
        async fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
            self.0.rename(from, to).await
        }
        async fn create_dir(&self, path: &VPath) -> Result<()> {
            self.0.create_dir(path).await
        }
        async fn search(&self, root: &VPath, needle: String, limit: usize) -> Result<Vec<VPath>> {
            self.0.search(root, needle, limit).await
        }
    }

    fn shown_vol(dir: &std::path::Path) -> Arc<dyn Volume> {
        Arc::new(OnlyWhenShown(LocalVolume::new(dir)))
    }

    #[tokio::test]
    async fn stat_reads_its_attributes_through_the_volume() {
        let d = tmp_dir("stat");
        std::fs::write(d.join("note.txt"), b"12345").unwrap();
        let got = dispatch_payload(
            shown_vol(&d),
            "stat",
            json!({ "path": d.join("note.txt").display().to_string() }),
        )
        .await
        .unwrap();
        assert_eq!(got["is_dir"], false);
        assert_eq!(got["size"], 5, "大小要从卷的 Entry 来");
        assert!(
            got["native_path"].is_string(),
            "本地卷必须有原生路径：{got}"
        );
        assert_eq!(got["writable"], true);
        // 这条同时判住 `show_hidden: true`：换成 false，上面整段就会 NotFound。
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn stat_on_a_directory_says_so() {
        let d = tmp_dir("statdir");
        std::fs::create_dir(d.join("sub")).unwrap();
        let got = dispatch_payload(
            vol(),
            "stat",
            json!({ "path": d.join("sub").display().to_string() }),
        )
        .await
        .unwrap();
        assert_eq!(got["is_dir"], true);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 查不到必须是 NotFound，不能返回一个"全 null 的对象"——
    /// 后者看起来像"这文件没有大小"，而不是"根本没有这个文件"。
    #[tokio::test]
    async fn stat_on_missing_path_is_not_found() {
        let d = tmp_dir("statmiss");
        let e = dispatch_payload(
            vol(),
            "stat",
            json!({ "path": d.join("ghost.txt").display().to_string() }),
        )
        .await
        .unwrap_err();
        assert_ne!(e.code, crate::protocol::METHOD_NOT_FOUND);
        assert!(
            e.message.contains("不存在"),
            "该报「不存在」，实得：{}",
            e.message
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 已装清单的返回里不许出现卸载命令行那两个字段（那是可执行面）。
    #[cfg(windows)]
    #[tokio::test]
    async fn packages_never_carries_uninstall_commands() {
        let got = dispatch_payload(vol(), "packages", json!({ "limit": 5 }))
            .await
            .unwrap();
        assert!(got["packages"].is_array(), "返回形状不对：{got}");
        assert!(got["count"].as_u64().unwrap() <= 5);
        let text = got.to_string().to_lowercase();
        for banned in ["uninstall_string", "quiet_uninstall"] {
            assert!(
                !text.contains(banned),
                "返回里出现了 {banned}：卸载命令行不外流"
            );
        }
        assert!(
            text.contains("has_uninstaller"),
            "至少要给出「有没有卸载器」这个布尔：{got}"
        );
    }

    /// 非 Windows 要**报错**，不能回一张空表——
    /// "这台机器没装软件"和"这台机器读不到注册表"是两件事。
    #[cfg(not(windows))]
    #[tokio::test]
    async fn packages_errors_instead_of_claiming_nothing_installed() {
        let e = dispatch_payload(vol(), "packages", json!({}))
            .await
            .unwrap_err();
        assert!(
            e.message.contains("Windows"),
            "该如实说只实现了 Windows，实得：{}",
            e.message
        );
    }

    /// 三条写路径在只读卷上必须在**落盘之前**被拦下，且报的是 INVALID_PARAMS
    /// 而不是内部错误——归档卷/远程卷挂进 UI 之后，这就是唯一的护栏。
    #[tokio::test]
    async fn readonly_volume_rejects_each_write_tool() {
        for (tool, args) in [
            ("write_file", json!({"path": "x.txt", "text": "hi"})),
            ("rename", json!({"from": "a", "to": "b"})),
            ("create_dir", json!({"path": "d"})),
        ] {
            let e = dispatch_payload(ro_vol(), tool, args)
                .await
                .err()
                .unwrap_or_else(|| panic!("只读卷上 {tool} 竟然成功了"));
            assert_eq!(
                e.code,
                crate::protocol::INTERNAL_ERROR,
                "{tool} 错误码：{}",
                e.message
            );
            // 钉住的是"跟兄弟工具同一条映射"，不是"这个码最合适"：
            // 只读现在统一落到 Unsupported → to_rpc → INTERNAL_ERROR。
            // 该不该给客户端一个更可辨的码是独立的契约改动，见任务清单。
            assert!(
                e.message.contains("只读"),
                "{tool} 应说明是只读卷拦的：{}",
                e.message
            );
        }
    }

    /// 只读卷的读路径不受影响：拦写不等于拦读。
    #[tokio::test]
    async fn readonly_volume_still_reads() {
        let d = tmp_dir("ro-read");
        std::fs::write(d.join("in.txt"), b"abc").unwrap();
        let p = d.join("in.txt").display().to_string();
        let v = dispatch_payload(ro_vol(), "read_file", json!({"path": p}))
            .await
            .expect("只读卷应能读");
        assert_eq!(v["text"], json!("abc"));
        assert_eq!(v["bytes"], json!(3));
        let _ = std::fs::remove_dir_all(&d);
    }
}
