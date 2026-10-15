use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
/// 外部进程状态的存活上限（毫秒）。过期后 UI 自动熄灭角标，必要时移除。
const EXTERNAL_OPEN_TTL_MS: u64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalOpen {
    /// 哪个应用打开了它（例如 "notepad.exe"）
    pub program: String,
    /// 启动时的进程句柄/PID。Windows: CreateProcess HANDLE 包装；Unix: PID。
    /// `None` 时只保存程序名（最少信息，隐私优先）。
    pub handle: Option<String>,
    /// 启动时间戳（毫秒），用于心跳超时熄灭。
    pub started_at_ms: u64,
}

impl ExternalOpen {
    /// 是否已过超时上限（未刷新即失活）。
    pub fn is_expired(&self) -> bool {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|now| now.as_millis() as u64)
            .unwrap_or(0)
            > self.started_at_ms + EXTERNAL_OPEN_TTL_MS
    }
}

#[derive(Debug, Default)]
pub struct ExternalOpens {
    map: std::sync::Mutex<Vec<(String, ExternalOpen)>>,
}

impl ExternalOpens {
    pub fn new() -> Self {
        Self { map: std::sync::Mutex::new(Vec::new()) }
    }

    /// 注册一个文件的外置打开（ID 不可与现有重复）。
    pub fn insert<N: Into<String>>(&self, file_id: N, open: ExternalOpen) {
        let mut map = self.map.lock().unwrap();
        map.retain(|(_, existing)| existing.handle != open.handle && existing.program != open.program);
        map.push((file_id.into(), open));
    }

    /// 按文件 ID 取状态。
    pub fn get<N: AsRef<str>>(&self, file_id: N) -> Option<ExternalOpen> {
        let map = self.map.lock().unwrap();
        map.iter()
            .find_map(|(id, o)| if id == file_id.as_ref() { Some(o.clone()) } else { None })
    }

    /// 按 PID 移除（进程退出）。返回被移除的条目。
    pub fn remove_by_handle(&self, handle: &str) -> Option<(String, ExternalOpen)> {
        let mut map = self.map.lock().unwrap();
        let idx = map.iter().position(|(_, o)| o.handle.as_deref() == Some(handle));
        idx.map(|i| map.remove(i))
    }

    pub fn is_empty(&self) -> bool {
        self.map.lock().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    /// 移除超时失活的条目。返回被移除的 ID 与状态。
    pub fn clear_expired(&self) -> Vec<(String, ExternalOpen)> {
        let mut map = self.map.lock().unwrap();
        let (expired, kept): (Vec<_>, Vec<_>) = map.drain(..).partition(|(_, o)| o.is_expired());
        *map = kept;
        expired
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
    async fn spawn_external_open(
        &self,
        file_path: &VPath,
        program: &str,
    ) -> Result<ExternalOpen>;

    /// 外部打开状态登记表（角标注入与心跳清理所用）。
    fn external_opens(&self) -> &ExternalOpens;

    /// 移除超时失活的外部打开条目；返回被移除的记录数。
    fn clear_expired(&self) -> usize;
}
