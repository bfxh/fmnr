pub mod board;
pub mod open;
pub mod search;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("other: {0}")]
    Other(String),
}

/// 卷的唯一标识，如 "local:C"、"wsl:Ubuntu"、"ssh:host"、"archive:/x/a.zip"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VolumeId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolumeKind {
    Local,
    Archive,
    Remote,
    Unc,
    Wsl,
}

/// 卷内路径：本地卷是真实 PathBuf；虚拟卷是卷内相对路径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VPath(pub PathBuf);

impl VPath {
    pub fn as_path(&self) -> &std::path::Path {
        self.0.as_path()
    }
}

/// 文件条目，UI 直接消费。
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<std::time::SystemTime>,
    /// 由 fmnr-icons 解析（hash/文件名/扩展名/内置 优先级链）
    pub icon_key: IconKey,
    pub attrs: Vec<Attr>,
}

impl Entry {
    pub fn extension(&self) -> Option<String> {
        std::path::Path::new(&self.name)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attr {
    Hidden,
    System,
    Symlink,
    ReadOnly,
}

/// 图标键：fmnr-icons 据此找覆盖或内置图标。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IconKey {
    ByExtension(String),
    ByName(String),
    ByHash(String),
    Directory,
    Generic,
}

/// 「已被外部程序打开」的状态。UI 角标 + MCP 快照都用这个。
///
/// 存活判定不在这里做：真正的判据是**外部进程是否已退出**（见
/// `Volume::spawn_external_open` 的实现与 `ExternalOpens::remove_by_handle`）。
/// 本类型只承载可序列化的数据，不含句柄（句柄不跨进程、也不该进 MCP 快照）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalOpen {
    /// 哪个应用打开了它（例如 "notepad.exe"）
    pub program: String,
    /// 启动时的进程句柄/PID。Windows: CreateProcess HANDLE 的十六进制串；Unix: PID。
    /// `None` 时只保存程序名（最少信息，隐私优先）。
    pub handle: Option<String>,
    /// 启动时间戳（毫秒），仅用于展示「何时打开」，不参与存活判定。
    pub started_at_ms: u64,
}

/// 外部打开状态登记表：文件名 → ExternalOpen。
#[derive(Debug, Default)]
pub struct ExternalOpens {
    map: std::sync::Mutex<Vec<(String, ExternalOpen)>>,
}

impl ExternalOpens {
    pub fn new() -> Self {
        Self {
            map: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 注册一个文件的外置打开。
    ///
    /// 同一文件重复打开只保留最新一条；不同文件可同时被同一程序打开，
    /// 不能按 `program` 去重（否则第二个文件的角标会被抹掉）。
    pub fn insert<N: Into<String>>(&self, file_id: N, open: ExternalOpen) {
        let id = file_id.into();
        let mut map = self.map.lock().unwrap();
        map.retain(|(existing_id, _)| existing_id != &id);
        map.push((id, open));
    }

    /// 按文件 ID 取状态。
    pub fn get<N: AsRef<str>>(&self, file_id: N) -> Option<ExternalOpen> {
        let map = self.map.lock().unwrap();
        map.iter().find_map(|(id, o)| {
            if id == file_id.as_ref() {
                Some(o.clone())
            } else {
                None
            }
        })
    }

    /// 进程退出时移除它对应的所有条目。
    /// 返回被移除的 (文件 ID, 状态)。
    pub fn remove_by_handle(&self, handle: &str) -> Vec<(String, ExternalOpen)> {
        let mut map = self.map.lock().unwrap();
        let mut gone = Vec::new();
        let mut kept = Vec::with_capacity(map.len());
        for (id, o) in map.drain(..) {
            if o.handle.as_deref() == Some(handle) {
                gone.push((id, o));
            } else {
                kept.push((id, o));
            }
        }
        *map = kept;
        gone
    }

    pub fn is_empty(&self) -> bool {
        self.map.lock().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn iter(&self) -> Vec<(String, ExternalOpen)> {
        self.map.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListOpts {
    pub show_hidden: bool,
    pub sort: SortOrder,
}

/// 目标已存在同名条目时的处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// 覆盖目标
    Overwrite,
    /// 跳过该条目（默认：不静默毁数据）
    #[default]
    Skip,
    /// 自动改名：`a.txt` → `a (2).txt`
    Rename,
}

/// 长操作（拷贝/移动）的进度计数。
/// 原子递增，UI 线程可无锁轮询；工作线程只写，UI 只读。
#[derive(Debug, Default)]
pub struct Progress {
    bytes_done: std::sync::atomic::AtomicU64,
    bytes_total: std::sync::atomic::AtomicU64,
    files_done: std::sync::atomic::AtomicU64,
    files_total: std::sync::atomic::AtomicU64,
    cancelled: std::sync::atomic::AtomicBool,
}

/// 进度快照，供 UI 单帧读取（避免多次原子读出现撕裂）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub cancelled: bool,
}

impl ProgressSnapshot {
    /// 完成比例 0.0..=1.0；总量未知（0）时返回 0.0。
    pub fn fraction(&self) -> f32 {
        if self.bytes_total == 0 {
            0.0
        } else {
            (self.bytes_done as f64 / self.bytes_total as f64).clamp(0.0, 1.0) as f32
        }
    }
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    /// 预声明总量（文件数与字节数），用于进度条分母。
    pub fn set_totals(&self, bytes: u64, files: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.bytes_total.store(bytes, Relaxed);
        self.files_total.store(files, Relaxed);
    }

    pub fn add_bytes(&self, n: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.bytes_done.fetch_add(n, Relaxed);
    }

    pub fn add_file(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.files_done.fetch_add(1, Relaxed);
    }

    /// 请求取消：工作线程在分块循环里轮询 `is_cancelled()`。
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        ProgressSnapshot {
            bytes_done: self.bytes_done.load(Relaxed),
            bytes_total: self.bytes_total.load(Relaxed),
            files_done: self.files_done.load(Relaxed),
            files_total: self.files_total.load(Relaxed),
            cancelled: self.cancelled.load(Relaxed),
        }
    }
}

/// 一次批量操作的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpReport {
    /// 成功处理的条目数
    pub done: usize,
    /// 因冲突策略跳过的条目数
    pub skipped: usize,
    /// 因用户取消而未处理的条目数
    pub aborted: usize,
    /// 实际搬运的字节数
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortOrder {
    #[default]
    Name,
    Size,
    Modified,
    Type,
}

/// 所有「能列出/读/写的命名空间」的统一抽象。
/// UI 和 MCP server 都通过它与数据交互，不直接碰文件系统。
#[async_trait::async_trait]
pub trait Volume: Send + Sync {
    fn id(&self) -> VolumeId;
    fn kind(&self) -> VolumeKind;

    /// 列出目录内容。
    async fn list(&self, path: &VPath, opts: &ListOpts) -> Result<Vec<Entry>>;

    /// 打开读流（用于预览/拷贝）。
    async fn read_stream(
        &self,
        path: &VPath,
    ) -> Result<Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>>;

    /// 写入（只读卷返回 Unsupported）。
    async fn write(&self, path: &VPath, data: &[u8]) -> Result<()>;

    fn writable(&self) -> bool;

    /// 原生路径：拖放/交给外部程序打开时用。虚拟卷返回 None。
    fn native_path(&self, path: &VPath) -> Option<PathBuf>;

    /// 让外部程序打开指定文件，记录进程句柄/程序名、启动时间。
    /// 只读卷（归档、远程）返回 Unsupported。返回失败原因。
    async fn spawn_external_open(&self, file_path: &VPath, program: &str) -> Result<ExternalOpen>;

    /// 外部打开状态登记表（角标注入与退出清理所用）。
    fn external_opens(&self) -> &ExternalOpens;

    // ---- M2 文件操作：全部走卷抽象，UI 不直接碰 std::fs ----

    /// 拷贝一组条目到目标目录。只读卷返回 Unsupported。
    /// 进度写入 `progress`；操作在工作线程上跑，调用方可 `Arc::clone` 后另起线程轮询。
    async fn copy(
        &self,
        sources: &[VPath],
        dest_dir: &VPath,
        policy: ConflictPolicy,
        progress: Arc<Progress>,
    ) -> Result<OpReport>;

    /// 移动一组条目到目标目录。跨卷时退化为「拷贝 + 删除源」。
    async fn move_entries(
        &self,
        sources: &[VPath],
        dest_dir: &VPath,
        policy: ConflictPolicy,
        progress: Arc<Progress>,
    ) -> Result<OpReport>;

    /// 删除一组条目。本地卷必须走回收站（可恢复），不可直接 unlink。
    async fn delete(&self, targets: &[VPath], progress: Arc<Progress>) -> Result<OpReport>;

    /// 重命名/原地移动单个条目（目标为完整新路径，含文件名）。
    async fn rename(&self, from: &VPath, to: &VPath) -> Result<()>;

    /// 新建目录。
    async fn create_dir(&self, path: &VPath) -> Result<()>;

    /// 在 `root` 子树内按**文件名子串**搜索（大小写不敏感），最多返回 `limit` 条。
    ///
    /// 这是给 MCP / CLI 这类「问一次要一批结果」的客户端用的有界接口。
    /// UI 另有一条流式变体（`fmnr-ui::search`），为的是「首屏 <500ms」口径下
    /// 先给用户看一批再补——两者命中判定必须一致，改动时一起改。
    async fn search(&self, root: &VPath, needle: String, limit: usize) -> Result<Vec<VPath>>;
}
