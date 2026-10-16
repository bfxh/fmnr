//! 手工验证入口：`cargo run -p fmnr-pkg --example list`
//!
//! 单测锁的是解析器（拿真实输出当语料），跑不到 spawn / 编码 / 退出码这一段，
//! 所以留这一条给开发机上一条命令量它。CI 不跑它（ubuntu 没有 winget）。

fn main() {
    match fmnr_pkg::list_upgrades() {
        Ok(rows) => {
            println!("{} 项可升级", rows.len());
            for r in rows {
                println!(
                    "  {} [{}] {} → {} ({})",
                    r.name, r.id, r.installed, r.available, r.source
                );
            }
        }
        Err(e) => {
            println!("盘点失败：{e}");
            std::process::exit(2);
        }
    }
}
