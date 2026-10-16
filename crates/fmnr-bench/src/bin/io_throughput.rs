//! 拷贝/移动的**吞吐**基线尺（与 `main.rs` 那把「UI 不卡」尺是两回事）。
//!
//! 为什么单独一把：`fmnr-bench` 原有那把量的是「拷 1GB 时 UI 帧率 ≥30fps」，
//! 它**不关心搬得多快** —— 一个每小时 1 MB 的实现只要不占 UI 也能过。
//! 要谈「加速」就得先有 MB/s 的数，否则改完无从对账。
//!
//! 用法（A/B 靠改代码前后各跑一次同一个场景）：
//! ```text
//! cargo run -p fmnr-bench --bin io_throughput -- --scenario copy-big
//! cargo run -p fmnr-bench --bin io_throughput -- --scenario all --root D:/tmp/fmnr-io
//! ```
//!
//! 口径与诚实边界：
//! - 每个场景**自己造新的源文件**，不复用，避免前一个场景把数据读进系统缓存
//!   让后一个虚高。同一文件被 OS 缓存后测出来的不是磁盘速度。
//! - 源与目标**同卷**（都在这块盘上）。跨卷/网络盘是另一道题，别混着报。
//! - 只报**墙钟吞吐**。真正的磁盘饱和要看队列深度与 CPU，这里不判。
//! - move 场景刻意用一棵**子树**而不是单文件：单文件 move 走 `rename` 是 O(1)，
//!   量的话会量到元数据操作，看不出「同卷移动是否误做了整树拷贝」这个真问题。

use fmnr_core::{ConflictPolicy, Progress};
use fmnr_fs::ops;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const MIB: u64 = 1 << 20;

/// 一个场景的测量结果。
struct Sample {
    name: &'static str,
    bytes: u64,
    secs: f64,
}

impl Sample {
    fn mibs(&self) -> f64 {
        (self.bytes as f64 / MIB as f64) / self.secs
    }
}

fn arg(flags: &[String], key: &str, default: &str) -> String {
    flags
        .iter()
        .position(|f| f == key)
        .and_then(|i| flags.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// 造一个确定内容的文件（不是稀疏的——稀疏只会量到元数据速度）。
fn write_file(path: &Path, len: u64) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut f = std::fs::File::create(path)?;
    let block: Vec<u8> = (0..64u8).cycle().take(1 << 16).collect();
    let mut left = len as usize;
    while left > 0 {
        let n = left.min(block.len());
        f.write_all(&block[..n])?;
        left -= n;
    }
    f.flush()?;
    drop(f);
    Ok(())
}

/// **参照天花板**：同一块盘、同样大小的新文件，用 `std::fs::copy` 走 OS 快速路径。
///
/// 这把尺不测 fmnr，测的是「这台机器这块盘在这个状态下最多能多快」。
/// 没有它，「113 MiB/s 太慢」只是一句没有参照的话——
/// 尤其这块盘只剩 10 GB / 1 TB，接近满载的无缓存 SSD 写速本来就会崩。
/// 判读规则：
/// - 参照 ≈ 我们的数 ⇒ 瓶颈在盘，别在代码里找。
/// - 参照 ≫ 我们的数 ⇒ 瓶颈在我们的读写字节循环，值得改。
fn ref_os_copy(root: &Path) -> Result<Sample, Box<dyn std::error::Error>> {
    let src = root.join("refsrc/blob.bin");
    let dst = root.join("out-ref");
    std::fs::create_dir_all(&dst)?;
    write_file(&src, 512 * MIB)?;
    let target = dst.join("blob.bin");

    let t = Instant::now();
    let copied = std::fs::copy(&src, &target)?;
    let secs = t.elapsed().as_secs_f64();
    Ok(Sample {
        name: "参照 std::fs::copy 1×512MiB",
        bytes: copied,
        secs,
    })
}

/// 参照天花板（小文件档）：同样 2000×64KiB，逐个用 `std::fs::copy` 顺序搬。
///
/// 用途是**分清瓶颈**：51 MiB/s 只有大文件档 113 MiB/s 的一半，
/// 可能是每文件固定开销（open/create/close）在拖，也可能是盘做随机小写就这样。
/// 参照≈我们的数 ⇒ 盘限制，并行也没用；参照≫ ⇒ 我们的 per-file 开销值得改。
fn ref_copy_many(root: &Path) -> Result<Sample, Box<dyn std::error::Error>> {
    let src_dir = root.join("refmany");
    let dst = root.join("out-refmany");
    std::fs::create_dir_all(&dst)?;
    let n = 2000usize;
    for i in 0..n {
        write_file(&src_dir.join(format!("f{i:04}.bin")), 64 * MIB / 1024)?;
    }
    // 字节数从 `std::fs::copy` 的返回值累加，不按 `n × 64KiB` 反推：
    // 反推的话万一某个 copy 少搬了，参照数会虚高。
    let t = Instant::now();
    let mut acc = 0u64;
    for i in 0..n {
        let s = src_dir.join(format!("f{i:04}.bin"));
        acc += std::fs::copy(&s, dst.join(format!("f{i:04}.bin")))?;
    }
    let secs = t.elapsed().as_secs_f64();
    Ok(Sample {
        name: "参照 std::fs::copy 2000×64KiB",
        bytes: acc,
        secs,
    })
}

fn copy_big(root: &Path) -> Result<Sample, Box<dyn std::error::Error>> {
    let src = root.join("big/blob.bin");
    let dst = root.join("out-big");
    write_file(&src, 512 * MIB)?;
    let bytes = 512 * MIB;

    let t = Instant::now();
    run_copy(&[src], &dst)?;
    let secs = t.elapsed().as_secs_f64();
    Ok(Sample {
        name: "copy 1×512MiB 单文件",
        bytes,
        secs,
    })
}

fn copy_many(root: &Path) -> Result<Sample, Box<dyn std::error::Error>> {
    let src_dir = root.join("many");
    let dst = root.join("out-many");
    // 2000 × 64KiB：这是「每文件固定开销」被放大的形状，
    // 单线程顺序做与并行做的差别在这里最明显。
    let n = 2000usize;
    for i in 0..n {
        write_file(&src_dir.join(format!("f{i:04}.bin")), 64 * MIB / 1024)?;
    }
    let bytes = (n as u64) * 64 * MIB / 1024;

    let t = Instant::now();
    run_copy(&[src_dir], &dst)?;
    let secs = t.elapsed().as_secs_f64();
    Ok(Sample {
        name: "copy 2000×64KiB 小文件",
        bytes,
        secs,
    })
}

fn move_tree(root: &Path) -> Result<Sample, Box<dyn std::error::Error>> {
    let src_dir = root.join("movetree");
    let dst = root.join("out-move");
    // 4 个子目录 × 64 MiB：同卷移动本该是一次 rename 元数据操作。
    for i in 0..4 {
        write_file(&src_dir.join(format!("sub{i}")).join("blob.bin"), 64 * MIB)?;
    }
    let bytes = 4 * 64 * MIB;

    let t = Instant::now();
    run_move(&[src_dir], &dst)?;
    let secs = t.elapsed().as_secs_f64();
    Ok(Sample {
        name: "move 同卷子树 4×64MiB",
        bytes,
        secs,
    })
}

fn run_copy(sources: &[PathBuf], dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let p = Arc::new(Progress::new());
    ops::transfer(sources, dest, ConflictPolicy::Skip, &p, false)?;
    Ok(())
}

fn run_move(sources: &[PathBuf], dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let p = Arc::new(Progress::new());
    ops::transfer(sources, dest, ConflictPolicy::Skip, &p, true)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flags: Vec<String> = std::env::args().skip(1).collect();
    let scenario = arg(&flags, "--scenario", "all");
    let root = PathBuf::from(arg(&flags, "--root", "D:/tmp/fmnr-io-bench"));

    std::fs::create_dir_all(&root)?;
    // 每轮从干净状态开始：残留的 out-* 会让冲突策略静默跳过，量出来是 0 字节。
    for sub in ["out-big", "out-many", "out-move", "big", "many", "movetree"] {
        let p = root.join(sub);
        if p.exists() {
            let _ = std::fs::remove_dir_all(&p);
        }
    }

    let mut samples = Vec::new();
    let run = |name: &str,
               f: fn(&Path) -> Result<Sample, Box<dyn std::error::Error>>,
               samples: &mut Vec<Sample>|
     -> Result<(), Box<dyn std::error::Error>> {
        if scenario == "all" || scenario == name {
            samples.push(f(&root)?);
        }
        Ok(())
    };

    run("copy-big", copy_big, &mut samples)?;
    run("copy-many", copy_many, &mut samples)?;
    run("move-tree", move_tree, &mut samples)?;
    run("ref-copy", ref_os_copy, &mut samples)?;
    run("ref-many", ref_copy_many, &mut samples)?;
    if samples.is_empty() {
        eprintln!("没有场景被跑到（--scenario 给了不认识的值？）");
        std::process::exit(2);
    }

    println!("# fmnr IO 吞吐基线  root={root:?}");
    for s in &samples {
        println!(
            "{:<28} {:>8.1} MiB  {:>7.3} s  {:>9.1} MiB/s",
            s.name,
            s.bytes as f64 / MIB as f64,
            s.secs,
            s.mibs()
        );
    }
    Ok(())
}
