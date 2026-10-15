//! 后台任务桥：操作跑在阻塞线程上，结果经 channel 回 UI 线程。
//! UI 每帧只 `try_recv` 一次 + 读一次原子快照，绝不阻塞。

use fmnr_core::{ConflictPolicy, OpReport, Progress, ProgressSnapshot, VPath, Volume};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{mpsc, Arc};

/// 后台任务种类。UI 用它决定完成后刷新哪一栏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpKind {
    Copy,
    Move,
    Delete,
}

#[derive(Debug)]
pub enum OpOutcome {
    Done(OpKind, OpReport),
    Failed(OpKind, String),
}

/// 运行中的长操作：进度句柄 + 完成信号。
pub struct RunningOp {
    pub kind: OpKind,
    pub label: String,
    pub progress: Arc<Progress>,
    rx: Receiver<OpOutcome>,
}

impl RunningOp {
    /// 每帧轮询：拿到完成结果就返回 Some，否则 None。
    pub fn poll(&self) -> Option<OpOutcome> {
        match self.rx.try_recv() {
            Ok(o) => Some(o),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(OpOutcome::Failed(
                self.kind.clone(),
                "工作线程异常退出".into(),
            )),
        }
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        self.progress.snapshot()
    }

    pub fn cancel(&self) {
        self.progress.cancel();
    }

    /// 进度条文案。字节数已知给字节，否则给文件数。
    pub fn bar_text(&self, s: ProgressSnapshot) -> String {
        if s.cancelled {
            return "正在中止…".into();
        }
        let unit = |b: u64| {
            const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
            let mut v = b as f64;
            let mut i = 0;
            while v >= 1024.0 && i + 1 < UNITS.len() {
                v /= 1024.0;
                i += 1;
            }
            format!("{v:.1} {}", UNITS[i])
        };
        if s.bytes_total > 0 {
            format!(
                "{} / {}  ({}/{} 个文件)",
                unit(s.bytes_done),
                unit(s.bytes_total),
                s.files_done,
                s.files_total
            )
        } else {
            format!("{} 个文件", s.files_done)
        }
    }
}

pub struct OpRequest {
    pub kind: OpKind,
    pub sources: Vec<VPath>,
    pub dest_dir: Option<VPath>,
    pub policy: ConflictPolicy,
}

/// 在独立线程上跑一个操作，立刻把 `RunningOp` 交回 UI。
pub fn spawn_op(
    volume: Arc<dyn Volume>,
    req: OpRequest,
) -> Result<RunningOp, String> {
    let (tx, rx): (Sender<OpOutcome>, Receiver<OpOutcome>) = mpsc::channel();
    let progress = Arc::new(Progress::new());
    let kind = req.kind.clone();

    let label = describe(&req);
    let worker_kind = kind.clone();
    let worker_progress = Arc::clone(&progress);

    // tokio runtime 在后台线程里建；操作本身全阻塞，不会占 UI。
    std::thread::Builder::new()
        .name("fmnr-op".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(OpOutcome::Failed(worker_kind, format!("runtime: {e}")));
                    return;
                }
            };
            let result = rt.block_on(async move {
                match req.kind {
                    OpKind::Copy => {
                        let dest = req
                            .dest_dir
                            .ok_or_else(|| fmnr_core::Error::Other("缺少目标目录".into()))?;
                        volume
                            .copy(&req.sources, &dest, req.policy, worker_progress)
                            .await
                    }
                    OpKind::Move => {
                        let dest = req
                            .dest_dir
                            .ok_or_else(|| fmnr_core::Error::Other("缺少目标目录".into()))?;
                        volume
                            .move_entries(&req.sources, &dest, req.policy, worker_progress)
                            .await
                    }
                    OpKind::Delete => volume.delete(&req.sources, worker_progress).await,
                }
            });
            let _ = tx.send(match result {
                Ok(r) => OpOutcome::Done(worker_kind, r),
                Err(e) => OpOutcome::Failed(worker_kind, e.to_string()),
            });
        })
        .map_err(|e| e.to_string())?;

    Ok(RunningOp {
        kind,
        label,
        progress,
        rx,
    })
}

fn describe(req: &OpRequest) -> String {
    let names: Vec<String> = req
        .sources
        .iter()
        .map(|p| {
            p.as_path()
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.as_path().display().to_string())
        })
        .collect();
    let verb = match req.kind {
        OpKind::Copy => "复制",
        OpKind::Move => "移动",
        OpKind::Delete => "删除",
    };
    let what = match names.len() {
        0 => "（空）".to_string(),
        1 => names[0].clone(),
        n => format!("{} 等 {} 项", names[0], n),
    };
    match &req.dest_dir {
        Some(d) => format!("{verb} {what} → {}", d.as_path().display()),
        None => format!("{verb} {what}"),
    }
}

/// 剪贴板：复制/剪切两态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipMode {
    Copy,
    Cut,
}

#[derive(Debug, Default)]
pub struct Clipboard {
    pub mode: Option<ClipMode>,
    pub paths: Vec<PathBuf>,
}

impl Clipboard {
    pub fn put(&mut self, mode: ClipMode, paths: Vec<PathBuf>) {
        self.mode = Some(mode);
        self.paths = paths;
    }

    pub fn clear(&mut self) {
        self.mode = None;
        self.paths.clear();
    }

    pub fn summary(&self) -> String {
        if self.paths.is_empty() {
            return "剪贴板为空".into();
        }
        let verb = match self.mode {
            Some(ClipMode::Copy) => "复制",
            Some(ClipMode::Cut) => "剪切",
            None => "剪贴板",
        };
        match self.paths.len() {
            1 => format!("{verb}：{}", self.paths[0].display()),
            n => format!("{verb}：{} 等 {n} 项", self.paths[0].display()),
        }
    }

    /// 剪贴板是否还有效：剪切模式下源必须还在（被别的程序挪走就失效）。
    pub fn still_valid(&self) -> bool {
        if self.mode == Some(ClipMode::Cut) {
            self.paths.iter().all(|p| p.exists())
        } else {
            !self.paths.is_empty()
        }
    }
}
