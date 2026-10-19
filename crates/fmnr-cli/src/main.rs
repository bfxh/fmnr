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
use fmnr_core::Volume;
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

    /// 查单个条目的属性（类型/大小/修改时间/隐藏·系统·符号链接·只读）
    Stat {
        /// 条目路径
        path: String,
    },

    /// 列已装程序底账（只读盘点：不含卸载命令行，也不代跑卸载或升级）
    Apps {
        /// 按名称或发行商子串筛
        #[arg(long)]
        query: Option<String>,
        /// 最多返回条数
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },

    /// 拷贝条目到目标目录（`--to` 可以给另一个卷：带卷名前缀或一条裸绝对路径 ⇒ 跨卷搬运）
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

    /// 批量：stdin 一行一条 `{"tool":"…","args":{…}}`，stdout 一行一条结果（NDJSON）。
    /// 给脚本和 AI 客户端用——共享**一次**挂载扫描，不必每条命令重开一个进程。
    Batch,
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
    Register {
        exe: Option<String>,
        dry_run: bool,
    },
    Unregister {
        dry_run: bool,
    },
    /// 批量：工具名从 stdin 里读，不在子命令表里写死。
    Batch,
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
            Cmd::Stat { path } => Action::Tool("stat", json!({ "path": path })),
            Cmd::Apps { query, limit } => {
                let mut a = json!({ "limit": limit });
                if let Some(q) = query {
                    a["query"] = json!(q);
                }
                Action::Tool("packages", a)
            }
            Cmd::Register { exe, dry_run } => Action::Register {
                exe: exe.clone(),
                dry_run: *dry_run,
            },
            Cmd::Unregister { dry_run } => Action::Unregister { dry_run: *dry_run },
            Cmd::Batch => Action::Batch,
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
        Action::Batch => run_batch(),
        Action::Tool(tool, args) => run_tool(tool, args),
    }
}

/// 挂载表 → `list_volume` 的返回体。
///
/// 单独抽出来是因为**两条路径都要它**：`fmnr volumes` 和 `fmnr batch` 里的一行。
/// 各写一份就会分叉成"batch 里的 list_volume 只报默认卷"——那正是这张表
/// 一路在避免的事。
fn volumes_payload(registry: &VolumeRegistry) -> Value {
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
    json!({ "volumes": items })
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

/// 一次进程生命周期里要复用的三件套：挂载表、默认卷、异步运行时。
///
/// 抽出来给 `run_tool` 和 `run_batch` 共用，理由就是批量模式存在的理由：
/// **挂载表只扫一次**。原来每条命令都要重开进程 + 重扫一遍挂载。
struct Ctx {
    registry: VolumeRegistry,
    default_volume: Arc<dyn Volume>,
    rt: tokio::runtime::Runtime,
}

impl Ctx {
    fn open() -> Self {
        let (registry, mount_failures) = build_volume_registry();

        // 挂载失败只进 stderr：stdout 是外部客户端消费的纯 JSON，混入诊断
        // 文本会直接把契约打碎——但不显示就等于吞错，所以分流不丢弃。
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

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|e| {
                eprintln!("fmnr: 无法启动运行时：{e}");
                std::process::exit(1);
            });

        Ctx {
            registry,
            default_volume,
            rt,
        }
    }

    /// 卷路由：路径带 `archive:`/`wsl:` 等前缀时切到对应卷，否则用默认卷。
    ///
    /// `copy`/`move` 的**源**只按 `sources` 选卷（规则在 `VolumeRegistry::route_source`，
    /// 与 MCP/HTTP 同一份）——把 `dest_dir` 也当样本会让"目标根比源根更深"时选错源卷。
    fn volume_for(&self, tool: &str, args: &Value) -> Arc<dyn Volume> {
        self.registry
            .route_source(tool, args)
            .unwrap_or_else(|| self.default_volume.clone())
    }
}

fn run_tool(tool: &'static str, args: Value) {
    let ctx = Ctx::open();
    if tool == "list_volume" {
        // 列出注册表里的**每一个**卷，而不是分发表里那个只报默认卷的臂。
        println!(
            "{}",
            serde_json::to_string_pretty(&volumes_payload(&ctx.registry)).unwrap()
        );
        return;
    }
    let volume = ctx.volume_for(tool, &args);
    let target = tools::cross_target(&ctx.registry, tool, &args, &volume);

    match ctx
        .rt
        .block_on(tools::dispatch_payload_to(volume, target, tool, args))
    {
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

/// 批量模式：stdin 一行一条 `{"tool":"…","args":{…}}`，stdout 一行一条结果。
///
/// 三条入口里 CLI 的摩擦最大：一条命令 = 一次进程 spawn + 一次重扫挂载，
/// 二十次就是二十次。这里挂载表只扫一次（`Ctx::open`），循环里只剩分派本身。
///
/// 三个刻意的形状：
/// - **输出是紧凑 NDJSON，不 pretty**：一行一条才叫流，缩进只会让 `| jq -c` 白跑。
/// - **一行失败不中断整批**：第 7 行拼错工具名，前 6 行和后 13 行的结果照样要给出去。
///   每条返回带 `line`，所以"空行不回话"也不会让消费方错位。
/// - **退出码：全成 0，有任何一行失败 1**，汇总计数走 stderr（不污染 stdout）。
fn run_batch() {
    use std::io::{BufRead, Write};
    let ctx = Ctx::open();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut handled = 0usize;
    let mut failed = 0usize;

    let mut emit = |v: Value| {
        if let Err(e) = writeln!(out, "{v}") {
            eprintln!("fmnr: 写 stdout 失败：{e}");
            std::process::exit(1);
        }
    };

    for (i, raw) in stdin.lock().lines().enumerate() {
        let line_no = i + 1;
        let line = match raw {
            Ok(l) => l,
            Err(e) => {
                failed += 1;
                emit(json!({"line": line_no, "ok": false,
                            "error": {"message": format!("读 stdin 第 {line_no} 行失败：{e}")}}));
                continue;
            }
        };
        // 空行是分隔符，不是一条请求，也不回一行。
        if line.trim().is_empty() {
            continue;
        }
        handled += 1;
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                failed += 1;
                emit(json!({"line": line_no, "ok": false,
                            "error": {"message": format!("第 {line_no} 行不是合法 JSON：{e}")}}));
                continue;
            }
        };
        let Some(tool) = req.get("tool").and_then(|t| t.as_str()) else {
            failed += 1;
            emit(json!({"line": line_no, "ok": false,
                        "error": {"message": format!(
                            "第 {line_no} 行缺 tool 字段（形状是 {{\"tool\":\"…\",\"args\":{{…}}}}）")}}));
            continue;
        };
        // `args` 省略、或显式给 null，都当"这个工具不收参数"。
        let args = match req.get("args") {
            Some(v) if !v.is_null() => v.clone(),
            _ => json!({}),
        };
        // `list_volume` 走的是挂载表而不是分发表：分发表那一臂只知道被选中的
        // 那个卷，批量里如果直接喂给它，同一条命令在两种模式下会给出不同答案。
        let result = if tool == "list_volume" {
            Ok(volumes_payload(&ctx.registry))
        } else {
            let volume = ctx.volume_for(tool, &args);
            let target = tools::cross_target(&ctx.registry, tool, &args, &volume);
            ctx.rt
                .block_on(tools::dispatch_payload_to(volume, target, tool, args))
        };
        match result {
            Ok(payload) => emit(json!({"line": line_no, "ok": true, "result": payload})),
            Err(e) => {
                failed += 1;
                emit(json!({"line": line_no, "ok": false,
                            "error": {"code": e.code, "message": e.message}}));
            }
        }
    }

    eprintln!("fmnr: 批量处理 {handled} 条，失败 {failed} 条");
    if failed > 0 {
        std::process::exit(1);
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

    /// `stat` 与 `apps` 的**参数形状**。工具名是否注册由
    /// `every_subcommand_targets_a_registered_tool` 统一判，这里只钉两件事：
    /// 子命令名和工具名不必一致（`apps` → `packages`），以及省略 query 时不许
    /// 塞一个空字段进去（`deny_unknown_fields` 那边会按"给了但为空"理解）。
    #[test]
    fn stat_and_apps_build_the_right_arguments() {
        let (t1, a1) = call(Cmd::Stat {
            path: r"D:\x\a.txt".into(),
        });
        assert_eq!(t1, "stat");
        assert_eq!(a1, json!({"path": r"D:\x\a.txt"}));
        let (t2, a2) = call(Cmd::Apps {
            query: None,
            limit: 3,
        });
        assert_eq!(t2, "packages");
        assert_eq!(a2, json!({"limit": 3}), "没给 query 时不该出现这个字段");
    }

    /// 给了 query 就要带上，且不能顺手把 limit 挤掉。
    #[test]
    fn apps_carries_query_when_given() {
        let (_, args) = call(Cmd::Apps {
            query: Some("zip".into()),
            limit: 200,
        });
        assert_eq!(args, json!({"limit": 200, "query": "zip"}));
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
    ///
    /// 这份名单**必须覆盖全部子命令**——底下那条数量断言就是钉这件事的：
    /// 新增一个 `Cmd` 变体而没往这里加一项，clap 数出来的子命令数就会和名单对不上，
    /// 当场判红。没有那条断言的话，"每个子命令"这个测试名是假的
    /// （N1 加 cat/put/rename/mkdir 时它就漏在名单外过）。
    #[test]
    fn every_subcommand_targets_a_registered_tool() {
        use clap::CommandFactory;
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
            Cmd::Cat {
                path: "a".into(),
                max_bytes: 1,
            },
            Cmd::Put {
                path: "a".into(),
                text: "b".into(),
            },
            Cmd::Rename {
                from: "a".into(),
                to: "b".into(),
            },
            Cmd::Mkdir { path: "a".into() },
            Cmd::Stat { path: "a".into() },
            Cmd::Apps {
                query: None,
                limit: 1,
            },
        ];
        let listed = cmds.len();
        for c in cmds {
            let (tool, _) = call(c);
            assert!(names.contains(&tool), "CLI 指向了未注册的工具：{tool}");
        }
        // 名单要盖住 clap 声明的**全部**子命令。三个不走工具表的豁免项：
        // `register`/`unregister`（系统边界，见
        // `registration_is_local_only_not_a_tool_call`）与 `batch`
        // （工具名从 stdin 读，不在这里写死）。
        const NOT_A_TOOL_CALL: usize = 3;
        let declared = Cli::command().get_subcommands().count();
        assert_eq!(
            declared,
            listed + NOT_A_TOOL_CALL,
            "clap 声明了 {declared} 个子命令（去掉 {NOT_A_TOOL_CALL} 个非工具调用该是 {} 个），\
             但这份名单只有 {listed} 项——新增了变体没往名单里加",
            declared - NOT_A_TOOL_CALL
        );
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
