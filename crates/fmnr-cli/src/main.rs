//! `fmnr` 命令行入口。
//!
//! **不另开一条文件系统通路**：每个子命令都翻译成一次
//! `fmnr_ai::tools::dispatch_payload` 调用——与 MCP server 同一个 match。
//! 否则会出现「MCP 能删、CLI 不能」这类分叉，而两边本该是同一份行为。
//!
//! 输出：结果 JSON 走 stdout（便于 `| jq`），错误走 stderr；出错退出码 1。

use clap::{Parser, Subcommand, ValueEnum};
use fmnr_ai::tools;
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

impl Cmd {
    /// 子命令 → (工具名, 工具参数)。纯映射，不碰 IO，便于单测。
    fn to_call(&self) -> (&'static str, Value) {
        match self {
            Cmd::Volumes => ("list_volume", json!({})),
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
                ("list_dir", a)
            }
            Cmd::Search { root, query, limit } => (
                "search",
                json!({ "root": root, "query": query, "limit": limit }),
            ),
            Cmd::Cp {
                sources,
                to,
                conflict,
            } => (
                "copy",
                json!({ "sources": sources, "dest_dir": to, "conflict": conflict.as_str() }),
            ),
            Cmd::Mv {
                sources,
                to,
                conflict,
            } => (
                "move",
                json!({ "sources": sources, "dest_dir": to, "conflict": conflict.as_str() }),
            ),
            Cmd::Rm { targets } => ("delete", json!({ "targets": targets })),
            Cmd::Open {
                path,
                external,
                program,
            } => {
                let mut a = json!({ "path": path, "force_external": external });
                if let Some(p) = program {
                    a["program"] = json!(p);
                }
                ("open", a)
            }
            Cmd::Opens => ("external_opens", json!({})),
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

fn main() {
    let cli = Cli::parse();
    let (tool, args) = cli.cmd.to_call();

    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let volume = Arc::new(LocalVolume::new(root));

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

    fn call(cmd: Cmd) -> (&'static str, Value) {
        cmd.to_call()
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
            let (tool, _) = c.to_call();
            assert!(names.contains(&tool), "CLI 指向了未注册的工具：{tool}");
        }
    }
}
