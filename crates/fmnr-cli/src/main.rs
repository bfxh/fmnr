//! `fmnr` 命令行入口。
//!
//! **不另开一条文件系统通路**：每个子命令都翻译成一次
//! `fmnr_ai::tools::dispatch_payload` 调用——与 MCP server 同一个 match。
//! 否则会出现「MCP 能删、CLI 不能」这类分叉，而两边本该是同一份行为。
//!
//! 输出：结果 JSON 走 stdout（便于 `| jq`），错误走 stderr；出错退出码 1。

use clap::{Parser, Subcommand, ValueEnum};
use fmnr_ai::tools;
use fmnr_core::volreg::VolumeRegistry;
use fmnr_fs::LocalVolume;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "fmnr",
    version,
    about = "独立文件管理器（内核与 UI/MCP 共用）",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 列出已挂载的卷
    Volumes,

    /// 列出目录内容
    Ls {
        /// 目录路径；省略则列卷根
        path: Option<String>,
        /// 显示隐藏项
        #[arg(short = 'a', long)]
        all: bool,
        /// 排序字段
        #[arg(long, value_enum, default_value_t = SortArg::Name)]
        sort: SortArg,
    },

    /// 在目录子树内按文件名子串搜索（大小写不敏感）
    Search {
        /// 搜索起点目录
        root: String,
        /// 文件名子串
        query: String,
        /// 最多返回条数
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },

    /// 读文件的文本内容（非 UTF-8 只报 binary，不吐乱码）
    Cat {
        /// 文件路径
        path: String,
        /// 读取上限（字节）
        #[arg(long, default_value_t = 262144)]
        max_bytes: usize,
    },

    /// 写一个 UTF-8 文本文件（整体覆盖）
    Put {
        /// 文件路径
        path: String,
        /// 完整内容
        text: String,
    },

    /// 重命名/移动单个条目（同卷内）
    Rename {
        /// 现路径
        from: String,
        /// 新路径
        to: String,
    },

    /// 新建目录
    Mkdir {
        /// 目录路径
        path: String,
    },

    /// 拷贝条目到目标目录
    Cp {
        /// 源条目（至少一个）
        #[arg(required = true)]
        sources: Vec<String>,
        /// 目标目录
        #[arg(long)]
        to: String,
        /// 同名时的策略
        #[arg(long, value_enum, default_value_t = ConflictArg::Skip)]
        conflict: ConflictArg,
    },

    /// 移动条目到目标目录
    Mv {
        #[arg(required = true)]
        sources: Vec<String>,
        #[arg(long)]
        to: String,
        #[arg(long, value_enum, default_value_t = ConflictArg::Skip)]
        conflict: ConflictArg,
    },

    /// 删除条目（本地卷走回收站，可恢复）
    Rm {
        #[arg(required = true)]
        targets: Vec<String>,
    },

    /// 按打开决策表打开文件：可预览类型走内置预览，否则启动外部程序
    Open {
        /// 文件路径
        path: String,
        /// 即使是可预览类型也强制用外部程序
        #[arg(long)]
        external: bool,
        /// 指定外部程序；省略用系统默认
        #[arg(long)]
        program: Option<String>,
    },

    /// 当前被外部程序打开的文件（角标状态）
    Opens,

    /// 把 fmnr 注册为「打开方式」候选，并注册 fmnr:// 协议（写 HKCU，免管理员）
    ///
    /// 只加入「打开方式」列表，**不改动系统默认程序**；不注册 COM。
    Register {
        /// 要注册的可执行文件路径；省略则用当前进程自身
        #[arg(long)]
        exe: Option<String>,
        /// 只打印将要写的内容，不真写注册表
        #[arg(long)]
        dry_run: bool,
    },

    /// 撤销上面那次注册，把 fmnr 建的所有键值删干净（幂等）
    Unregister {
        /// 只打印将要删除的内容，不真删
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SortArg {
    Name,
    Size,
    Modified,
    Type,
}

#[derive(Clone, Copy, ValueEnum)]
enum ConflictArg {
    Overwrite,
    Skip,
    Rename,
}

/// 子命令的两类去向。
///
/// 大多数子命令是**一次工具调用**（与 MCP 共用 dispatch）；
/// `register`/`unregister` 不是工具调用，而是本机的注册表动作——
/// 它们碰的是系统边界，不该被远程客户端经由 MCP/HTTP 触发。
enum Action {
    Tool(&'static str, Value),
    Register { exe: Option<String>, dry_run: bool },
    Unregister { dry_run: bool },
}

impl Cmd {
    /// 子命令 → 去向。纯映射，不碰 IO，便于单测。
    fn to_action(&self) -> Action {
        match self {
            Cmd::Volumes => Action::Tool("list_volume", json!({})),
            Cmd::Ls { path, all, sort } => {
                let mut a = json!({ "show_hidden": all });
                if let Some(p) = path {
                    a["path"] = json!(p);
                }
                a["sort"] = json!(match sort {
                    SortArg::Name => "name",
                    SortArg::Size => "size",
                    SortArg::Modified => "modified",
                    SortArg::Type => "type",
                });
                Action::Tool("list_dir", a)
            }
            Cmd::Search { root, query, limit } => Action::Tool(
                "search",
                json!({ "root": root, "query": query, "limit": limit }),
            ),
            Cmd::Cp {
                sources,
                to,
                conflict,
            } => Action::Tool(
                "copy",
                json!({ "sources": sources, "dest_dir": to, "conflict": conflict.as_str() }),
            ),
            Cmd::Mv {
                sources,
                to,
                conflict,
            } => Action::Tool(
                "move",
                json!({ "sources": sources, "dest_dir": to, "conflict": conflict.as_str() }),
            ),
            Cmd::Rm { targets } => Action::Tool("delete", json!({ "targets": targets })),
            Cmd::Open {
                path,
                external,
                program,
            } => {
                let mut a = json!({ "path": path, "force_external": external });
                if let Some(p) = program {
                    a["program"] = json!(p);
                }
                Action::Tool("open", a)
            }
            Cmd::Opens => Action::Tool("external_opens", json!({})),
            Cmd::Cat { path, max_bytes } => {
                Action::Tool("read_file", json!({ "path": path, "max_bytes": max_bytes }))
            }
            Cmd::Put { path, text } => {
                Action::Tool("write_file", json!({ "path": path, "text": text }))
            }
            Cmd::Rename { from, to } => Action::Tool("rename", json!({ "from": from, "to": to })),
            Cmd::Mkdir { path } => Action::Tool("create_dir", json!({ "path": path })),
            Cmd::Register { exe, dry_run } => Action::Register {
                exe: exe.clone(),
                dry_run: *dry_run,
            },
            Cmd::Unregister { dry_run } => Action::Unregister { dry_run: *dry_run },
        }
    }
}

impl ConflictArg {
    fn as_str(&self) -> &'static str {
        match self {
            ConflictArg::Overwrite => "overwrite",
            ConflictArg::Skip => "skip",
            ConflictArg::Rename => "rename",
        }
    }
}

/// 把「当前工作目录」组装成一个卷注册表：本地卷 + 当目
/// 录下所有可识别的归档（zip/tar），让 CLI 的工具调用跑在统一的仓上。
///
/// **M8 入口**，与 UI 侧共用 `fmnr_mount` 的同一套挂载逻辑（`default_plan` +
/// `mount_all`），避免两处各自拼归档后缀表、悄悄分叉。挂载失败的
/// **原因**随 tuple 返回，由调用方决定显示到 stderr——不吞错。
fn build_volume_registry() -> (VolumeRegistry, Vec<String>) {
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let reg = VolumeRegistry::new();
    let specs = fmnr_mount::default_plan(&root, Some(&root));
    let mut table = fmnr_mount::MountTable::new();
    let failures = table.mount_all(&reg, &specs);
    (reg, failures)
}

fn main() {
    let cli = Cli::parse();

    match cli.cmd.to_action() {
        Action::Register { exe, dry_run } => run_register(exe, dry_run),
        Action::Unregister { dry_run } => run_unregister(dry_run),
        Action::Tool(tool, args) => run_tool(tool, args),
    }
}

/// 注册：只写 HKCU。默认拿当前进程自身当目标程序。
fn run_register(exe: Option<String>, dry_run: bool) {
    let exe = match exe {
        Some(e) => e,
        None => match std::env::current_exe() {
            Ok(p) => p.display().to_string(),
            Err(e) => {
                eprintln!("fmnr: 拿不到自身路径：{e}");
                std::process::exit(1);
            }
        },
    };
    let plan = fmnr_fs::assoc::RegistrationPlan::build(&exe);

    if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan_to_json(&plan)).unwrap()
        );
        return;
    }

    match fmnr_fs::assoc::apply(&plan) {
        Ok(r) => println!(
            "{}",
            serde_json::json!({ "ok": true, "written": r.written, "exe": exe })
        ),
        Err(e) => {
            eprintln!("fmnr: 注册失败：{e}");
            std::process::exit(1);
        }
    }
}

/// 撤销：删干净 fmnr 建的键值。共享键只摘自己的值。
fn run_unregister(dry_run: bool) {
    // 撤销不依赖 exe 路径：要删的键名是固定的。
    let plan = fmnr_fs::assoc::RegistrationPlan::build("fmnr.exe");

    if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan_to_json(&plan)).unwrap()
        );
        return;
    }

    match fmnr_fs::assoc::revert(&plan) {
        Ok(r) => println!(
            "{}",
            serde_json::json!({ "ok": true, "removed": r.removed })
        ),
        Err(e) => {
            eprintln!("fmnr: 撤销失败：{e}");
            std::process::exit(1);
        }
    }
}

/// 把注册计划摊平成 JSON，供 `--dry-run` 展示「到底会写什么」。
fn plan_to_json(plan: &fmnr_fs::assoc::RegistrationPlan) -> Value {
    let ops: Vec<Value> = plan
        .ops
        .iter()
        .map(|o| {
            let v = match &o.value {
                fmnr_fs::assoc::RegValue::Sz(s) => s.clone(),
                fmnr_fs::assoc::RegValue::ExpandSz(s) => s.clone(),
            };
            json!({ "key": format!("HKCU\\{}", o.subkey), "name": o.name, "value": v })
        })
        .collect();
    let shared: Vec<Value> = plan
        .shared()
        .iter()
        .map(|(k, n)| json!({ "key": format!("HKCU\\{k}"), "value_to_remove": n }))
        .collect();
    json!({
        "will_write": ops,
        "shared_values_we_would_remove": shared,
        "owned_keys_we_would_delete": plan.subkeys(),
    })
}

fn run_tool(tool: &'static str, args: Value) {
    let (registry, mount_failures) = build_volume_registry();

    // 挂载失败只进 stderr：stdout 是外部客户端消费的纯 JSON，混入诊断
    // 文本会直接把 MCP 契约打碎——但不显示就等于吞错，所以分流不丢弃。
    for f in &mount_failures {
        eprintln!("fmnr: 挂载失败：{f}");
    }

    // 默认卷 = 注册表第一个。**不硬编码 id**：LocalVolume::new(cwd) 自生的
    // id 是 local:<cwd 绝对路径>，写 local:. 永远查不到（与 UI 侧同款的坑），
    // 只能靠兜底新建卷，注册表就白建了。
    let default_volume = registry
        .list()
        .first()
        .and_then(|v| registry.get(&v.id))
        .unwrap_or_else(|| {
            Arc::new(LocalVolume::new(
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            ))
        });

    let volume = match tool {
        // `list_volume` 列出注册表里的每一个卷。
        "list_volume" => {
            let items: Vec<Value> = registry
                .list()
                .iter()
                .map(|v| {
                    json!({
                        "id": v.id.0,
                        "kind": format!("{:?}", v.kind),
                        "writable": v.writable,
                        "name": v.name,
                        "note": v.note,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "volumes": items })).unwrap()
            );
            return;
        }
        // 卷路由：路径带 archive:/wsl: 等前缀时切到对应卷，否则用默认卷。
        _ => registry.route_for_args(&args).unwrap_or(default_volume),
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fmnr: 无法启动运行时：{e}");
            std::process::exit(1);
        }
    };

    match rt.block_on(tools::dispatch_payload(volume, tool, args)) {
        Ok(payload) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
            );
        }
        Err(e) => {
            // 错误进 stderr，stdout 保持「只有结果 JSON」，管道友好。
            eprintln!("fmnr: {} ({})", e.message, e.code);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取工具调用；不是工具调用就 panic（调用点自己知道该是哪类）。
    fn call(cmd: Cmd) -> (&'static str, Value) {
        match cmd.to_action() {
            Action::Tool(t, a) => (t, a),
            _ => panic!("该子命令不是工具调用"),
        }
    }

    #[test]
    fn ls_maps_to_list_dir_with_sort() {
        let (tool, args) = call(Cmd::Ls {
            path: Some("D:\\x".into()),
            all: true,
            sort: SortArg::Size,
        });
        assert_eq!(tool, "list_dir");
        assert_eq!(args["path"], json!("D:\\x"));
        assert_eq!(args["show_hidden"], json!(true));
        assert_eq!(args["sort"], json!("size"));
    }

    /// 省略 path 时不该塞一个空字符串进去——那会被解析成「相对根」，
    /// 而语义是「列卷根」，两者对 LocalVolume 恰好等价，但对其他卷未必。
    #[test]
    fn ls_without_path_omits_the_field() {
        let (_, args) = call(Cmd::Ls {
            path: None,
            all: false,
            sort: SortArg::Name,
        });
        assert!(
            args.get("path").is_none(),
            "省略路径时不该出现 path：{args}"
        );
    }

    #[test]
    fn open_without_program_omits_the_field() {
        let (tool, args) = call(Cmd::Open {
            path: "a.txt".into(),
            external: false,
            program: None,
        });
        assert_eq!(tool, "open");
        assert!(
            args.get("program").is_none(),
            "未指定程序时不该出现 program"
        );
        assert_eq!(args["force_external"], json!(false));
    }

    #[test]
    fn conflict_flags_lowercase() {
        let (_, args) = call(Cmd::Cp {
            sources: vec!["a".into()],
            to: "d".into(),
            conflict: ConflictArg::Overwrite,
        });
        assert_eq!(args["conflict"], json!("overwrite"));
    }

    /// 每个子命令都必须映射到真实存在的工具，否则 CLI 会拿到 -32601。
    #[test]
    fn every_subcommand_targets_a_registered_tool() {
        let names: Vec<&str> = tools::specs().iter().map(|s| s.name).collect();
        let cmds = [
            Cmd::Volumes,
            Cmd::Ls {
                path: None,
                all: false,
                sort: SortArg::Name,
            },
            Cmd::Search {
                root: ".".into(),
                query: "a".into(),
                limit: 1,
            },
            Cmd::Cp {
                sources: vec!["a".into()],
                to: "b".into(),
                conflict: ConflictArg::Skip,
            },
            Cmd::Mv {
                sources: vec!["a".into()],
                to: "b".into(),
                conflict: ConflictArg::Skip,
            },
            Cmd::Rm {
                targets: vec!["a".into()],
            },
            Cmd::Open {
                path: "a".into(),
                external: false,
                program: None,
            },
            Cmd::Opens,
        ];
        for c in cmds {
            let (tool, _) = call(c);
            assert!(names.contains(&tool), "CLI 指向了未注册的工具：{tool}");
        }
    }

    /// register/unregister 必须**不是**工具调用。
    ///
    /// 它们改的是本机注册表。如果做成工具，远程 MCP/HTTP 客户端就能
    /// 顺着协议改用户系统的默认程序——那是系统边界，只能由本机命令行触发。
    #[test]
    fn registration_is_local_only_not_a_tool_call() {
        assert!(matches!(
            Cmd::Register {
                exe: None,
                dry_run: true
            }
            .to_action(),
            Action::Register { .. }
        ));
        assert!(matches!(
            Cmd::Unregister { dry_run: true }.to_action(),
            Action::Unregister { .. }
        ));

        // 反向确认：这两个名字不在 MCP 工具表里
        let names: Vec<&str> = tools::specs().iter().map(|s| s.name).collect();
        assert!(!names.contains(&"register"), "register 不该是 MCP 工具");
        assert!(!names.contains(&"unregister"), "unregister 不该是 MCP 工具");
    }
}
