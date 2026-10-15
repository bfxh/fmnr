use fmnr_core::{
    Attr, Entry, Error, ListOpts, Result, SortOrder, VPath, Volume, VolumeId, VolumeKind,
};
use std::path::{Path, PathBuf};

/// 本地文件系统卷：一个盘符/挂载点。核心枚举在进程内完成，不 shell out。
pub struct LocalVolume {
    id: VolumeId,
    root: PathBuf,
}

impl LocalVolume {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let id = VolumeId(format!("local:{}", root.display()));
        Self { id, root }
    }

    /// 把卷内路径解析为真实文件系统路径。
    fn resolve(&self, path: &VPath) -> PathBuf {
        if path.0.is_absolute() {
            path.0.clone()
        } else {
            self.root.join(&path.0)
        }
    }
}

#[async_trait::async_trait]
impl Volume for LocalVolume {
    fn id(&self) -> VolumeId {
        self.id.clone()
    }

    fn kind(&self) -> VolumeKind {
        VolumeKind::Local
    }

    async fn list(&self, path: &VPath, opts: &ListOpts) -> Result<Vec<Entry>> {
        let dir = self.resolve(path);
        let read_dir = std::fs::read_dir(&dir)
            .map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("{}: {e}", dir.display()))))?;

        let mut entries = Vec::new();
        for item in read_dir {
            let item = item?;
            let name = item.file_name().to_string_lossy().to_string();
            let meta = match item.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            let mut attrs = Vec::new();
            if !opts.show_hidden {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    let fa = meta.file_attributes();
                    if fa & 0x2 != 0 {
                        // FILE_ATTRIBUTE_HIDDEN
                        if !opts.show_hidden {
                            continue;
                        }
                        attrs.push(Attr::Hidden);
                    }
                    if fa & 0x4 != 0 {
                        attrs.push(Attr::System);
                    }
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let mode = meta.mode();
                    if name.starts_with('.') {
                        continue;
                    }
                    if mode & 0o400 == 0 {
                        attrs.push(Attr::ReadOnly);
                    }
                }
            }

            let symlink = item.path().is_symlink();
            if symlink {
                attrs.push(Attr::Symlink);
            }

            let icon_key = match Path::new(&item.file_name().to_string_lossy().to_string())
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
            {
                Some(ext) if !ext.is_empty() => fmnr_core::IconKey::ByExtension(ext),
                _ => fmnr_core::IconKey::Generic,
            };

            entries.push(Entry {
                name,
                is_dir: meta.is_dir(),
                size: if meta.is_file() { Some(meta.len()) } else { None },
                modified: meta.modified().ok(),
                icon_key,
                attrs,
            });
        }

        sort_entries(&mut entries, opts.sort);
        Ok(entries)
    }

    async fn read_stream(
        &self,
        path: &VPath,
    ) -> Result<Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>> {
        let full = self.resolve(path);
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(&full))
            .await
            .map_err(|e| Error::Other(e.to_string()))?
            .map_err(Error::Io)?;
        let chunk = bytes.to_vec();
        let stream = futures::stream::iter(std::iter::once(Ok(chunk)));
        Ok(Box::new(stream))
    }

    async fn write(&self, path: &VPath, data: &[u8]) -> Result<()> {
        let full = self.resolve(path);
        tokio::fs::write(&full, data).await.map_err(Error::Io)
    }

    fn writable(&self) -> bool {
        true
    }

    fn native_path(&self, path: &VPath) -> Option<PathBuf> {
        Some(self.resolve(path))
    }
}

fn sort_entries(entries: &mut [Entry], order: SortOrder) {
    match order {
        SortOrder::Name => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        SortOrder::Size => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)))
        }),
        SortOrder::Modified => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| b.modified.cmp(&a.modified))
        }),
        SortOrder::Type => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.extension().cmp(&b.extension()))
        }),
    }
}

/// 即时搜索：在当前子树用谓词过滤（jwalk 并行）。
pub fn search(
    root: &Path,
    predicate: impl Fn(&Entry) -> bool + Send + Sync + 'static,
) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    for entry in jwalk::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let e = Entry {
                name: entry.file_name().to_string_lossy().to_string(),
                is_dir: false,
                size: entry.metadata().ok().map(|m| m.len()),
                modified: None,
                icon_key: fmnr_core::IconKey::Generic,
                attrs: Vec::new(),
            };
            if predicate(&e) {
                hits.push(entry.path());
            }
        }
    }
    hits
}
