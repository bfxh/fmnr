use fmnr_core::{
    Attr, Entry, Error, ExternalOpen, ExternalOpens, ListOpts, Result, SortOrder, VPath, Volume,
    VolumeId, VolumeKind,
};
use std::path::{Path, PathBuf};

/// 本地文件系统卷：一个盘符/挂载点。核心枚举在进程内完成，不 shell out。
pub struct LocalVolume {
    id: VolumeId,
    root: PathBuf,
    /// 外部打开状态登记表（UI 角标 + 心跳移除共用）
    external_opens: ExternalOpens,
}

impl LocalVolume {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let id = VolumeId(format!("local:{}", root.display()));
        Self {
            id,
            root,
            external_opens: ExternalOpens::new(),
        }
    }

    fn resolve(&self, path: &VPath) -> PathBuf {
        if path.0.is_absolute() {
            path.0.clone()
        } else {
            self.root.join(&path.0)
        }
    }

    pub fn external_opens(&self) -> &ExternalOpens {
        &self.external_opens
    }

    pub fn external_opens_mut(&mut self) -> &mut ExternalOpens {
        &mut self.external_opens
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
        Ok(Box::new(stream) as Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>)
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

    fn external_opens(&self) -> &ExternalOpens {
        &self.external_opens
    }

    fn clear_expired(&self) -> usize {
        let before = self.external_opens.len();
        self.external_opens.clear_expired();
        before - self.external_opens.len()
    }

    /// 让外部程序打开指定文件，记录进程句柄/程序名、启动时间。
    /// Windows: 走 CreateProcessW，不走 ShellExecute；派生外部窗口可见状态。
    /// 只读卷（归档/远程）返回 Unsupported。
    async fn spawn_external_open(
        &self,
        file_path: &VPath,
        program: &str,
    ) -> Result<ExternalOpen> {
        let full = self.resolve(file_path);
        if !full.exists() {
            return Err(Error::NotFound(full.display().to_string()));
        }

        #[cfg(windows)]
        {
            use windows::Win32::Foundation::GetLastError;
            use windows::Win32::System::Threading::{
                CreateProcessW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
            };

            let prog = program
                .find(' ')
                .map(|i| &program[..i])
                .unwrap_or(program);
            let cmdline = format!("\"{}\" \"{}\"", prog, full.display());

            let prog_wide: Vec<u16> = prog.encode_utf16().chain(std::iter::once(0)).collect();
            let cmdline_wide: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

            let mut si = STARTUPINFOW::default();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut pi = PROCESS_INFORMATION::default();

            let ok = unsafe {
                CreateProcessW(
                    windows::core::PCWSTR(prog_wide.as_ptr()),
                    Some(windows::core::PWSTR(cmdline_wide.as_ptr() as *mut _)),
                    None,
                    None,
                    false,
                    CREATE_UNICODE_ENVIRONMENT,
                    None,
                    None,
                    &si,
                    &mut pi,
                )
                .is_ok()
            };
            if !ok {
                let err = unsafe { GetLastError() };
                return Err(Error::Other(format!("CreateProcess failed: {:?}", err)));
            }

            // HANDLE 转字符串，供日志/状态可见
            let handle = format!("0x{:x}", pi.hProcess.0 as usize);
            let started_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            Ok(ExternalOpen {
                program: prog.to_string(),
                handle: Some(handle),
                started_at_ms,
            })
        }

        #[cfg(not(windows))]
        {
            // Unix: 简化——直接在 UI 侧用 std::process 替代，暂不实现此契约。
            Err(Error::Unsupported(
                "Unix spawn_external_open 暂未实现，退回 ShellExecute".into(),
            ))
        }
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
