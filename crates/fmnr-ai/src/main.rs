//! `fmnr-ai` 可执行入口：默认把 stdio 接上 MCP server；`--serve` 时改起 HTTP API。
//!
//! stdout 是 MCP 协议通道，日志一律走 stderr（`tracing_subscriber` 默认写 stderr）。

use fmnr_ai::server::Server;
use fmnr_fs::LocalVolume;
use std::sync::Arc;

/// 极简参数解析：只认 `--serve <addr>` 与 `--allow-remote`。
/// 不引 clap——这个入口的参数面就这么大，多一个依赖不值当。
struct Args {
    serve: Option<String>,
    allow_remote: bool,
}

enum Parsed {
    Run(Args),
    Help,
}

fn parse_args(argv: Vec<String>) -> Result<Parsed, String> {
    let mut out = Args {
        serve: None,
        allow_remote: false,
    };
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--serve" => {
                let addr = it.next().ok_or("--serve 后面要跟地址，如 127.0.0.1:8787")?;
                out.serve = Some(addr);
            }
            "--allow-remote" => out.allow_remote = true,
            "--help" | "-h" => return Ok(Parsed::Help),
            other => return Err(format!("未知参数：{other}\n\n{}", usage())),
        }
    }
    Ok(Parsed::Run(out))
}

fn usage() -> String {
    "用法：\n  \
     fmnr-ai                  # stdio MCP server（默认）\n  \
     fmnr-ai --serve HOST:PORT [--allow-remote]\n\n\
     默认只允许绑回环地址；对外暴露需显式 --allow-remote。"
        .to_string()
}

fn main() -> std::io::Result<()> {
    // 日志去 stderr；stdout 只留给协议帧。
    // 级别固定 WARN：这是被客户端 spawn 的进程，日志多了只会淹没客户端日志面板。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();

    let args = match parse_args(std::env::args().skip(1).collect()) {
        Ok(Parsed::Run(a)) => a,
        Ok(Parsed::Help) => {
            println!("{}", usage());
            return Ok(());
        }
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    // 卷根：MCP 客户端没有「当前目录」概念，用启动时的工作目录做根。
    // 所有工具参数都给绝对路径，因此根只影响相对路径的解析。
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let volume = Arc::new(LocalVolume::new(root));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match args.serve {
        None => {
            // stdio MCP：单线程足够（一次一条帧），用 current_thread 省资源。
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let mut server = Server::new(volume);
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            rt.block_on(server.serve(stdin.lock(), stdout.lock()))
        }
        Some(raw_addr) => {
            let (host, port) = split_addr(&raw_addr);
            let addr = match fmnr_ai::http::bind_addr(&host, port, args.allow_remote) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            };
            eprintln!("fmnr-ai: HTTP API 监听 {addr}");
            rt.block_on(fmnr_ai::http::run(volume, &addr))
        }
    }
}

/// 拆 `host:port`。IPv6 字面量可能是 `[::1]:8787` 或裸 `::1`。
fn split_addr(s: &str) -> (String, u16) {
    if let Some(rest) = s.strip_prefix('[') {
        // [::1]:8787
        if let Some((h, p)) = rest.split_once("]:") {
            return (h.to_string(), p.parse().unwrap_or(8787));
        }
    }
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(8787)),
        None => (s.to_string(), 8787),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_means_stdio() {
        let Parsed::Run(a) = parse_args(vec![]).unwrap() else {
            panic!("无参数应是 Run");
        };
        assert!(a.serve.is_none());
        assert!(!a.allow_remote);
    }

    #[test]
    fn serve_takes_the_next_token() {
        let Parsed::Run(a) = parse_args(vec!["--serve".into(), "127.0.0.1:9000".into()]).unwrap()
        else {
            panic!("应是 Run");
        };
        assert_eq!(a.serve.as_deref(), Some("127.0.0.1:9000"));
    }

    #[test]
    fn help_is_not_an_error() {
        assert!(matches!(
            parse_args(vec!["--help".into()]),
            Ok(Parsed::Help)
        ));
    }

    #[test]
    fn serve_without_address_is_an_error() {
        assert!(parse_args(vec!["--serve".into()]).is_err());
    }

    #[test]
    fn unknown_flag_is_an_error() {
        assert!(parse_args(vec!["--wat".into()]).is_err());
    }

    #[test]
    fn split_addr_handles_ipv4_and_ipv6() {
        assert_eq!(split_addr("127.0.0.1:8787"), ("127.0.0.1".into(), 8787));
        assert_eq!(split_addr("[::1]:8787"), ("::1".into(), 8787));
    }
}
