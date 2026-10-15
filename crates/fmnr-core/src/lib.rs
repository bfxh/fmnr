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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IconKey {
    ByExtension(String),
    ByName(String),
    ByHash(String),
    Directory,
    Generic,
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
}
