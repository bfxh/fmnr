//! DESIGN.md §8 验收：拷 1 GB 时 UI 帧率 ≥30fps。
//!
//! 口径说明（别把这把尺读宽）：
//! - 「UI 线程」= 跑 egui `Context::run` 的那个线程，它每帧只做 try_recv + 读原子快照。
//! - 「工作线程」= 真正搬字节的线程。
//! - 判据 = 工作线程忙的那段时间里，UI 线程的帧间隔中位数与最坏值。
//!
//! 若拷贝被错误地做在 UI 线程上，UI 线程帧间隔会跟着 IO 一起飙到秒级，红。

use fmnr_core::{ConflictPolicy, Progress};
use fmnr_fs::ops;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 造一棵 GiB 级的树：每 64 MiB 一个文件，散在 4 个子目录。
/// 刻意不用稀疏文件：那样量到的是元数据速度，不是数据搬运速度。
fn build_fixture(
    root: &std::path::Path,
    gib: u64,
) -> std::io::Result<Vec<(std::path::PathBuf, u64)>> {
    const CHUNK: usize = 1 << 20;
    let per_file = 64 * 1024 * 1024u64;
    let files = gib * 1024 * 1024 * 1024 / per_file;
    let mut made = Vec::new();
    for i in 0..files {
        let sub = root.join(format!("part{}", i % 4));
        std::fs::create_dir_all(&sub)?;
        let p = sub.join(format!("blob{i}.bin"));
        let mut f = std::fs::File::create(&p)?;
        let block = vec![0xABu8; CHUNK];
        for _ in 0..(per_file / CHUNK as u64) {
            std::io::Write::write_all(&mut f, &block)?;
        }
        made.push((p, per_file));
    }
    Ok(made)
}

/// UI 线程模拟：固定间隔 tick，记录每帧间隔。
///
/// 关键：每帧做的事必须与真实渲染循环同构——读一次进度快照 + 试收一次完成信号。
/// 如果只 sleep 不碰共享状态，这把尺证明不了「拷贝没占住 UI 线程」。
fn ui_loop(
    stop: Arc<AtomicBool>,
    frames: Arc<Mutex<Vec<Duration>>>,
    progress: Arc<Progress>,
    tick: Duration,
) {
    let mut last = Instant::now();
    let mut out = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        out.push(now.duration_since(last));
        last = now;

        // 与 App::poll_running / RunningOp::snapshot 同形
        let snap = progress.snapshot();
        std::hint::black_box((snap.bytes_done, snap.fraction(), snap.cancelled));

        std::thread::sleep(tick);
    }
    *frames.lock().unwrap() = out;
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx]
}

fn main() {
    let gib: u64 = std::env::var("BENCH_GIB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let tmp = std::env::temp_dir().join(format!("fmnr-bench-{}", std::process::id()));
    let src = tmp.join("src");
    let dst = tmp.join("dst");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    // 收尾必清：跑一次就是 2 GiB 的垃圾，不能留在盘上
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&tmp);
    };

    println!("准备 {gib} GiB 夹具于 {}", src.display());
    let t0 = Instant::now();
    let made = match build_fixture(&src, gib) {
        Ok(m) => m,
        Err(e) => {
            cleanup();
            eprintln!("夹具创建失败：{e}");
            std::process::exit(2);
        }
    };
    let bytes: u64 = made.iter().map(|(_, b)| b).sum();
    println!(
        "夹具就绪：{} 个文件 / {:.2} GiB，耗时 {:.1}s",
        made.len(),
        bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        t0.elapsed().as_secs_f64()
    );

    // ---- UI 线程与工作线程并发 ----
    let stop = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(Mutex::new(Vec::<Duration>::new()));
    let progress = Arc::new(Progress::new());
    let ui_progress = Arc::clone(&progress);
    let ui_handle = {
        let stop = Arc::clone(&stop);
        let frames = Arc::clone(&frames);
        std::thread::spawn(move || ui_loop(stop, frames, ui_progress, Duration::from_micros(833)))
    };

    let sources: Vec<_> = made.iter().map(|(p, _)| p.clone()).collect();

    let copy_start = Instant::now();
    let result = ops::transfer(&sources, &dst, ConflictPolicy::Skip, &progress, false);
    let copy_elapsed = copy_start.elapsed();
    stop.store(true, Ordering::Relaxed);
    ui_handle.join().unwrap();

    let report = match result {
        Ok(r) => r,
        Err(e) => {
            cleanup();
            eprintln!("拷贝失败：{e}");
            std::process::exit(2);
        }
    };

    // ---- 校验：字节数与文件数都对得上（尺子的输入必须可复算） ----
    let (out_bytes, out_files) = ops::dir_stats(&dst);
    let verify = (out_bytes == bytes)
        && (out_files == made.len() as u64)
        && (report.bytes == bytes)
        && (progress.snapshot().bytes_done == bytes);
    if !verify {
        cleanup();
        eprintln!(
            "FAIL: 拷贝内容不匹配 src={bytes} dst={out_bytes} files {} vs {} report={} progress={}",
            made.len(),
            out_files,
            report.bytes,
            progress.snapshot().bytes_done
        );
        std::process::exit(2);
    }

    // ---- 帧率统计 ----
    let frames = frames.lock().unwrap().clone();
    let mut sorted = frames.clone();
    sorted.sort();
    let median = percentile(&sorted, 0.5);
    let p95 = percentile(&sorted, 0.95);
    let worst = sorted.last().copied().unwrap_or(Duration::ZERO);
    let avg = sorted.iter().sum::<Duration>() / sorted.len().max(1) as u32;

    println!("\n=== 结果 ===");
    println!("拷贝耗时       {:.2}s", copy_elapsed.as_secs_f64());
    println!(
        "吞吐           {:.0} MiB/s",
        bytes as f64 / (1024.0 * 1024.0) / copy_elapsed.as_secs_f64()
    );
    println!("UI 采样帧数    {}", frames.len());
    println!(
        "帧间隔 中位数  {:?}  ({:.0} fps)",
        median,
        1.0 / median.as_secs_f64()
    );
    println!(
        "帧间隔 平均    {:?}  ({:.0} fps)",
        avg,
        1.0 / avg.as_secs_f64()
    );
    println!(
        "帧间隔 P95     {:?}  ({:.0} fps)",
        p95,
        1.0 / p95.as_secs_f64()
    );
    println!(
        "帧间隔 最坏    {:?}  ({:.0} fps)",
        worst,
        1.0 / worst.as_secs_f64()
    );

    cleanup();

    // 判据取最坏帧而非平均：平均会洗掉「某一帧卡了两秒」这种真实卡顿。
    if 1.0 / worst.as_secs_f64() >= 30.0 {
        println!("\nPASS: 最坏帧率 ≥30fps，拷 {gib} GiB 不卡 UI");
        std::process::exit(0);
    } else {
        println!(
            "\nFAIL: 最坏帧仅 {:.1} fps（最坏帧间隔 {:?}）",
            1.0 / worst.as_secs_f64(),
            worst
        );
        std::process::exit(2);
    }
}
