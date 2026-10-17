use fmnr_core::{
    Attr, ConflictPolicy, Entry, Error, ExternalOpen, ExternalOpens, ListOpts, OpReport, Progress,
    Result, SortOrder, VPath, Volume, VolumeId, VolumeKind,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod assoc;
pub mod autostart;
pub mod default_prog;
pub mod ops;
pub mod space;
pub mod startup;

/// 本地文件系统卷：一个盘符/挂载点。核心枚举在进程内完成，不 shell out。
pub struct LocalVolume {
    id: VolumeId,
    root: PathBuf,
    /// 外部打开状态登记表（UI 角标读取；看门狗线程在进程退出时写入）
    external_opens: Arc<ExternalOpens>,
}

impl LocalVolume {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        // WSL UNC 的卷名用 `wsl:<distro>`（DESIGN §4.3 的命名），
        // 而不是把 `\\wsl.localhost\Ubuntu` 整条塞进 id——那会让同一
        // distro 的不同子目录看起来像不同的卷。
        let id = match fmnr_core::wsl::parse_wsl_unc(&root) {
            Some(u) => fmnr_core::wsl::wsl_volume_id(&u.distro),
            None => VolumeId(format!("local:{}", root.display())),
        };
        Self {
            id,
            root,
            external_opens: Arc::new(ExternalOpens::new()),
        }
    }

    fn resolve(&self, path: &VPath) -> PathBuf {
        if path.0.is_absolute() {
            path.0.clone()
        } else if let Some(rooted) = root_drive_relative(&path.0) {
            rooted
        } else {
            self.root.join(&path.0)
        }
    }

    pub fn external_opens(&self) -> &ExternalOpens {
        &self.external_opens
    }
}

/// Windows 上「有盘符、没有根」的写法（`E:`、`E:foo`）既不算绝对路径，
/// 也不是普通的相对路径：它按**该盘的当前目录**解析，而 `root.join("E:")`
/// 会把 `root` 整个丢掉——实测 `PathBuf::from("E:\\tmp").join("E:") == "E:"`，
/// 于是进程 CWD 在 `E:\tmp` 时"列 E 盘"实际列出的是 `E:\tmp`（1001 条），
/// 而 `E:\` 只有 17 条。栏头写着一个目录、列出的是另一个目录，
/// 用户照着栏头走 `cd`、照着行删文件，两边指向的不是同一处。
///
/// 这里把它补成盘根写法（`E:` → `E:\`、`E:foo` → `E:\foo`），
/// 与资源管理器地址栏的行为一致，且不随进程 CWD 变。
/// 非 Windows 上盘符前缀不存在，这个函数恒返回 None（不写 cfg 门控，
/// 免得两侧各长出一份死代码）。
fn root_drive_relative(p: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    let mut comps = p.components();
    let Some(Component::Prefix(pref)) = comps.next() else {
        return None;
    };
    // 这一版 std 的 `Prefix::Disk` 装的是 u8（盘符的 ASCII 码），
    // 直接 format 会打印成 `69:\`；必须先变回字符。
    let Prefix::Disk(code) = pref.kind() else {
        return None;
    };
    let drive = char::from(code);
    // `E:\` 这种已经带根的绝对写法到不了这里（is_absolute 为真），
    // 但防御性地再挡一次，避免哪天调用顺序变了把根吃掉。
    if matches!(comps.clone().next(), Some(Component::RootDir)) {
        return None;
    }
    let mut out = PathBuf::from(format!("{drive}:\\"));
    out.extend(comps);
    Some(out)
}

#[async_trait::async_trait]
impl Volume for LocalVolume {
    fn id(&self) -> VolumeId {
        self.id.clone()
    }

    fn kind(&self) -> VolumeKind {
        // WSL 的 UNC 在 Windows 侧就是可 read_dir 的普通目录，所以复用本实现；
        // 但**卷的类型必须如实报 Wsl**——图标管线靠它跳过 hash 覆盖
        // （DESIGN §4.4：远程/虚拟卷做 hash 代价太高）。
        if fmnr_core::wsl::looks_like_wsl_unc(&self.root) {
            VolumeKind::Wsl
        } else {
            VolumeKind::Local
        }
    }

    async fn list(&self, path: &VPath, opts: &ListOpts) -> Result<Vec<Entry>> {
        let dir = self.resolve(path);
        // 「路径不存在」是调用方给错了，不是本机的 IO 故障——要报 NotFound
        // 而不是 Io。否则 MCP/HTTP 会把它当服务端错误（500），
        // 客户端也就分不清「我路径写错了」和「服务炸了」。
        // 同文件的 `search` 早已这么做，这里对齐。
        let read_dir = std::fs::read_dir(&dir).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::NotFound(dir.display().to_string())
            } else {
                Error::Io(std::io::Error::new(
                    e.kind(),
                    format!("{}: {e}", dir.display()),
                ))
            }
        })?;

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

            let is_dir = meta.is_dir();
            let icon_key = if is_dir {
                fmnr_core::IconKey::Directory
            } else {
                match Path::new(&name)
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                {
                    Some(ext) if !ext.is_empty() => fmnr_core::IconKey::ByExtension(ext),
                    _ => fmnr_core::IconKey::Generic,
                }
            };

            entries.push(Entry {
                name,
                is_dir,
                size: if meta.is_file() {
                    Some(meta.len())
                } else {
                    None
                },
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
        Ok(Box::new(stream)
            as Box<
                dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin,
            >)
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

    /// 让外部程序打开指定文件，记录进程句柄/程序名、启动时间。
    /// Windows: 走 CreateProcessW，不走 ShellExecute；派生外部窗口可见状态。
    /// 只读卷（归档/远程）返回 Unsupported。
    async fn spawn_external_open(&self, file_path: &VPath, program: &str) -> Result<ExternalOpen> {
        let full = self.resolve(file_path);
        if !full.exists() {
            return Err(Error::NotFound(full.display().to_string()));
        }
        // 空程序 = 调用方希望"用系统默认程序"。这一条现在真的去查注册表了
        // （`default_prog::resolve_for`：UserChoice → HKCU/HKLM 的 Classes 映射
        // → `shell\open\command`），**只读不写**，不改任何默认值。
        //
        // 查不到仍然明确报错，不回退到 COMSPEC。以前那条会静默起一个永不退出的
        // cmd.exe：文件没打开、角标谎报"已在 cmd.exe 中打开"、还连带把 MCP 测试的
        // 临时目录钉在 %TEMP% 里删不掉。报错比"看起来做了点什么"值钱。
        let owned_program: String = if program.trim().is_empty() {
            let d = crate::default_prog::resolve_for(&full)?.ok_or_else(|| {
                Error::Unsupported(format!(
                    "没找到 {} 的系统默认程序：这台机器上这个扩展名没有可用的默认打开方式\
                     （非 Windows 平台目前不解析默认程序）。请显式传 program。",
                    full.display()
                ))
            })?;
            // 程序路径自己带空格时必须先裹引号，否则下面 `split_program`
            // 会把 `C:\Program Files\x\a.exe` 从中间切断。
            let prog = if d.program.contains(' ') {
                format!("\"{}\"", d.program)
            } else {
                d.program.clone()
            };
            if d.args.is_empty() {
                prog
            } else {
                format!("{prog} {}", d.args)
            }
        } else {
            program.to_string()
        };
        let program = owned_program.as_str();

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::Win32::Foundation::GetLastError;
            use windows::Win32::System::Threading::{
                CreateProcessW, CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT,
                PROCESS_INFORMATION, STARTUPINFOW,
            };

            // `CreateProcessW` 的 lpApplicationName **不搜索 PATH**：传 "cmd.exe"
            // 会直接 ERROR_FILE_NOT_FOUND。必须先自己解析成绝对路径。
            let (prog_raw, args) = split_program(program);
            let exe = resolve_program(&prog_raw)
                .ok_or_else(|| Error::NotFound(format!("找不到可执行文件：{prog_raw}")))?;

            let cmdline = if args.is_empty() {
                format!("\"{}\" \"{}\"", exe.display(), full.display())
            } else {
                format!("\"{}\" {} \"{}\"", exe.display(), args, full.display())
            };

            let exe_wide: Vec<u16> = exe
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let cmdline_wide: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

            let si = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                ..Default::default()
            };
            let mut pi = PROCESS_INFORMATION::default();

            // `CREATE_NEW_CONSOLE` 不是装饰：不指定 STARTF_USESTDHANDLES 时，
            // Windows **总会**把父进程的标准句柄复制给子进程（与 bInheritHandles
            // 无关）。对 stdio MCP server 来说那是致命的——子进程会往
            // stdout（JSON-RPC 通道）里写自己的横幅，客户端立刻收到非法帧。
            // 给子进程一个独立控制台，它就与我们的 stdio 彻底脱钩；
            // GUI 程序本来就没有控制台，该标志对其无影响（不会闪黑框）。
            let flags = CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_CONSOLE;

            let ok = unsafe {
                CreateProcessW(
                    windows::core::PCWSTR(exe_wide.as_ptr()),
                    Some(windows::core::PWSTR(cmdline_wide.as_ptr() as *mut _)),
                    None,
                    None,
                    false,
                    flags,
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

            // CreateProcess 给了两个句柄：主线程留一个，看门狗线程留一个。
            // 谁都不 CloseHandle 就是每次外置打开泄 2 个内核句柄。
            let process_handle = pi.hProcess;
            let thread_handle = pi.hThread;
            let key = format!("{:x}", process_handle.0 as usize);
            let started_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            // 线程句柄本线程用不上，立刻回收；进程句柄交给看门狗。
            unsafe { windows::Win32::Foundation::CloseHandle(thread_handle) }.ok();

            let opens = Arc::clone(&self.external_opens);
            let watch_key = key.clone();
            let owned = OwnedHandle(process_handle);
            if let Err(e) = std::thread::Builder::new()
                .name("fmnr-proc-watch".into())
                .spawn(move || watch_process(owned, watch_key, &opens))
            {
                // 线程没起来就没人 CloseHandle，句柄得就地回收。
                unsafe { windows::Win32::Foundation::CloseHandle(process_handle) }.ok();
                return Err(Error::Other(format!("watchdog 线程启动失败：{e}")));
            }

            Ok(ExternalOpen {
                // 展示用文件名（不含路径），角标文案更干净
                program: exe
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| exe.display().to_string()),
                handle: Some(key),
                started_at_ms,
            })
        }

        #[cfg(not(windows))]
        {
            // Unix 侧的这条契约**没实现**：拿不到进程句柄就没有可靠的存活判定，
            // 而「已在 XX 中打开」角标依赖它（见 DESIGN §4.7）。
            // 这里用 `std::process::Command` 起进程是**不够**的——它给 PID 不给
            // 与 Windows 等价的等待语义，且不解决「谁负责 reap」的问题；
            // 与其给一个半对的实现，不如显式报错让人看见。
            Err(Error::Unsupported(format!(
                "非 Windows 平台的 spawn_external_open 未实现：\
                 无法为 {} 记录 {program} 的进程存活，角标会失效",
                full.display()
            )))
        }
    }

    async fn copy(
        &self,
        sources: &[VPath],
        dest_dir: &VPath,
        policy: ConflictPolicy,
        progress: std::sync::Arc<Progress>,
    ) -> Result<OpReport> {
        let srcs = self.resolve_all(sources);
        let dest = self.resolve(dest_dir);
        spawn_blocking(move || ops::transfer(&srcs, &dest, policy, &progress, false)).await
    }

    async fn move_entries(
        &self,
        sources: &[VPath],
        dest_dir: &VPath,
        policy: ConflictPolicy,
        progress: std::sync::Arc<Progress>,
    ) -> Result<OpReport> {
        let srcs = self.resolve_all(sources);
        let dest = self.resolve(dest_dir);
        spawn_blocking(move || ops::transfer(&srcs, &dest, policy, &progress, true)).await
    }

    async fn delete(
        &self,
        targets: &[VPath],
        progress: std::sync::Arc<Progress>,
    ) -> Result<OpReport> {
        let paths = self.resolve_all(targets);
        let (bytes, files) = paths.iter().fold((0u64, 0u64), |acc, p| {
            let (b, f) = ops::dir_stats(p);
            (acc.0 + b, acc.1 + f)
        });
        progress.set_totals(bytes, files);

        let done = spawn_blocking(move || ops::delete_to_recycle_bin(&paths)).await?;
        // 删除是原子的：要么整批进回收站，要么报错，不存在部分完成。
        progress.add_bytes(bytes);
        for _ in 0..files {
            progress.add_file();
        }
        Ok(OpReport {
            done,
            skipped: 0,
            aborted: 0,
            bytes,
        })
    }

    async fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        let src = self.resolve(from);
        let dst = self.resolve(to);
        spawn_blocking(move || std::fs::rename(&src, &dst).map_err(Error::Io)).await
    }

    async fn create_dir(&self, path: &VPath) -> Result<()> {
        let full = self.resolve(path);
        spawn_blocking(move || std::fs::create_dir(&full).map_err(Error::Io)).await
    }

    async fn search(&self, root: &VPath, needle: String, limit: usize) -> Result<Vec<VPath>> {
        let dir = self.resolve(root);
        if !dir.is_dir() {
            return Err(Error::NotFound(format!("{} 不是目录", dir.display())));
        }
        let hits = spawn_blocking(move || {
            let mut out = Vec::new();
            for entry in jwalk::WalkDir::new(&dir)
                .skip_hidden(true)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if out.len() >= limit {
                    break;
                }
                let name = entry.file_name().to_string_lossy();
                if fmnr_core::search::name_matches(&name, &needle) {
                    out.push(VPath(entry.path()));
                }
            }
            Ok(out)
        })
        .await?;
        Ok(hits)
    }
}

impl LocalVolume {
    fn resolve_all(&self, paths: &[VPath]) -> Vec<PathBuf> {
        paths.iter().map(|p| self.resolve(p)).collect()
    }
}

/// 把「程序 + 可选参数」拆开。参数是程序自带时（如 `cmd.exe /c exit`）要保留，
/// 不能像早先那样在第一个空格处一刀切掉。
#[cfg(windows)]
fn split_program(program: &str) -> (String, String) {
    let p = program.trim();
    if let Some(rest) = p.strip_prefix('"') {
        // 引号形式："C:\Program Files\x\y.exe" args
        if let Some(end) = rest.find('"') {
            let exe = rest[..end].to_string();
            let args = rest[end + 1..].trim().to_string();
            return (exe, args);
        }
    }
    match p.split_once(' ') {
        Some((exe, args)) => (exe.to_string(), args.trim().to_string()),
        None => (p.to_string(), String::new()),
    }
}

/// 把程序名解析成绝对路径。
///
/// 搜索顺序刻意**不含当前目录**：`CreateProcessW` 的默认搜索会包含 CWD，
/// 那意味着在「下载目录」里放一个同名 `cmd.exe` 就能被优先执行——典型劫持面。
/// 这里只认「已是绝对路径且存在」与「PATH 中命中」。
#[cfg(windows)]
pub(crate) fn resolve_program(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }

    // 带路径分隔符但不是绝对路径（如 .\x.exe）：显式拒绝，避免相对路径歧义。
    if program.contains(['/', '\\']) {
        return None;
    }

    let path_var = std::env::var_os("PATH")?;
    // PATHEXT 决定无扩展名时补哪些后缀，默认取 .EXE
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();

    let has_ext = p.extension().is_some();
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if has_ext {
            let cand = dir.join(program);
            if cand.is_file() {
                return Some(cand);
            }
        } else {
            for ext in &exts {
                let cand = dir.join(format!("{program}{ext}"));
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// 看门狗：阻塞在 `WaitForSingleObject` 上，进程一退出就摘角标并回收句柄。
///
/// 用 INFINITE 而不是轮询：没人需要知道「还活着」，只需要知道「什么时候不再活着」。
/// 无论等待成功还是失败，都必须走到 `CloseHandle`——句柄泄漏是静默的，
/// 只有在进程句柄耗尽时才会以「无法启动外部程序」的形式暴露出来。
#[cfg(windows)]
fn watch_process(handle: OwnedHandle, key: String, opens: &ExternalOpens) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::WaitForSingleObject;

    // INFINITE = u32::MAX：不超时，直到进程结束才返回。
    unsafe { WaitForSingleObject(handle.0, u32::MAX) };
    unsafe { CloseHandle(handle.0) }.ok();
    opens.remove_by_handle(&key);
}

/// 已被移交给看门狗线程的进程句柄。
///
/// `HANDLE` 是裸 `*mut c_void`，不实现 `Send`。这里显式断言它可跨线程：
/// 它只是内核句柄表里的一个整数，**唯一性由进程保证**；移交后原线程不再触碰，
/// 由 `watch_process` 独占并在退出时 `CloseHandle`，不存在双重拥有。
#[cfg(windows)]
struct OwnedHandle(windows::Win32::Foundation::HANDLE);

// SAFETY: 见类型注释——单一所有者、无并发访问、仅作为整数传给 Win32 API。
#[cfg(windows)]
unsafe impl Send for OwnedHandle {}

/// 把同步 IO 丢到阻塞线程池，避免拖住 async 执行器。
async fn spawn_blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Other(e.to_string()))?
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

#[cfg(test)]
mod path_tests {
    use super::*;

    #[cfg(windows)]
    fn opts() -> ListOpts {
        ListOpts {
            show_hidden: false,
            sort: SortOrder::Name,
        }
    }

    #[cfg(windows)]
    fn names(v: &[Entry]) -> Vec<String> {
        let mut n: Vec<String> = v.iter().map(|e| e.name.clone()).collect();
        n.sort();
        n
    }

    /// 普通相对写法照旧接在卷根后面（三条腿共用的那条正常路径不能被改坏）。
    #[test]
    fn relative_path_still_joins_the_volume_root() {
        let root = std::env::temp_dir().join(format!("fmnr-rel-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let vol = LocalVolume::new(&root);
        assert_eq!(
            vol.resolve(&VPath(PathBuf::from("sub"))),
            root.join("sub"),
            "普通相对写法必须仍然接在卷根后"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    fn cwd_drive() -> char {
        let cwd = std::env::current_dir().expect("拿不到 CWD");
        match cwd.components().next() {
            Some(std::path::Component::Prefix(p)) => match p.kind() {
                std::path::Prefix::Disk(code) => char::from(code),
                other => panic!("这条测试要一个盘符开头的 CWD，实得 {other:?}"),
            },
            other => panic!("这条测试要一个盘符开头的 CWD，实得 {other:?}"),
        }
    }

    /// 回归：`<盘符>:` 这种盘相对写法**不许**跟着进程 CWD 走。
    ///
    /// 修前实测（CWD=`E:\tmp`）：`list("E:")` 返回 1001 条 = `E:\tmp` 的内容，
    /// 而 `list("E:\\")` 返回 17 条 = 盘根；差别的来源就是
    /// `root.join("E:")` 把 root 吃掉、Windows 按该盘当前目录解析 `E:`。
    #[cfg(windows)]
    #[test]
    fn drive_colon_only_lists_the_drive_root_not_the_process_cwd() {
        let d = cwd_drive();
        // 卷根故意是一个只含一个标记文件的临时目录：
        // 若盘相对写法还走 join 分支，列出来的就是它（1 条）而不是盘根。
        let root = std::env::temp_dir().join(format!("fmnr-drive-rel-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let marker = "fmnr-drive-relative-marker.txt";
        std::fs::write(root.join(marker), b"x").unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let vol = LocalVolume::new(&root);
        let rel = VPath(PathBuf::from(format!("{d}:")));
        let abs = VPath(PathBuf::from(format!("{d}:\\")));
        let got_rel = rt.block_on(vol.list(&rel, &opts())).expect("列盘根应成功");
        let got_abs = rt.block_on(vol.list(&abs, &opts())).expect("列盘根应成功");

        assert_eq!(
            names(&got_rel),
            names(&got_abs),
            "`{d}:` 与 `{d}:\\` 必须列同一份内容；前者跟着 CWD 走就是本次修的缺陷"
        );
        assert!(
            !names(&got_rel).iter().any(|n| n == marker),
            "列 `{d}:` 却列到了卷根的临时目录（marker 泄漏进结果）"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 盘相对写法带子路径时也要补根：`E:ghost-…` 必须报到盘根那条路径上去，
    /// 而不是 CWD 下的同名子目录——否则"不存在"这个结论本身就是错的。
    #[cfg(windows)]
    #[test]
    fn drive_relative_subpath_is_rooted_and_errors_at_the_right_place() {
        let d = cwd_drive();
        let ghost = format!("fmnr-drive-relative-ghost-{}", std::process::id());
        let vol = LocalVolume::new(std::env::temp_dir());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let e = rt
            .block_on(vol.list(&VPath(PathBuf::from(format!("{d}:{ghost}"))), &opts()))
            .expect_err("盘根下没有这个目录，必须报错");
        let msg = e.to_string();
        assert!(
            msg.contains(&format!("{d}:\\{ghost}")),
            "错误里要写出它去的是 `{d}:\\{ghost}`（而不是跟着 CWD），实得：{msg}"
        );
    }
}

#[cfg(all(test, windows))]
mod watch_tests {
    use super::*;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    /// 本进程当前的内核句柄数。
    fn handle_count() -> u32 {
        let mut n = 0u32;
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) }
            .expect("GetProcessHandleCount");
        n
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cond()
    }

    fn temp_file(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fmnr-watch-{tag}-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, b"x").unwrap();
        p
    }

    /// 判据：外部进程**真实退出**后，登记表里的条目自动消失。
    ///
    /// 这里用 `cmd /c exit` 造一个立刻结束的进程——它不是「过一会儿超时」，
    /// 而是真死。若实现退回 TTL，本测试会因为等不到 30s 而失败（测试超时窗口设 5s）。
    #[test]
    fn badge_clears_when_real_process_exits() {
        let vol = LocalVolume::new(std::env::temp_dir());
        let file = temp_file("exit");
        let rt = tokio::runtime::Runtime::new().unwrap();

        let open = rt
            .block_on(vol.spawn_external_open(&VPath(file.clone()), "cmd.exe /c exit"))
            .expect("spawn");
        let key = file.display().to_string();
        vol.external_opens().insert(key.clone(), open);
        assert_eq!(vol.external_opens().len(), 1, "打开后应有角标");

        // cmd /c exit 几乎立刻退出；给 5s 余量（远小于旧实现的 30s TTL）。
        let cleared = wait_until(
            || vol.external_opens().get(&key).is_none(),
            std::time::Duration::from_secs(5),
        );
        assert!(cleared, "进程已退出但角标仍在——存活判定没有跟随真实进程");
        assert_eq!(vol.external_opens().len(), 0);
        std::fs::remove_file(&file).ok();
    }

    /// 反证：进程**还在跑**时角标必须留着。
    /// 没有这条，一个「永远立刻返回」的实现也能过上面那条。
    ///
    /// 命令要满足两点：真的活 ~2 秒；且能把尾部追加的文件参数无害吞掉。
    /// 实测 `ping -n 3 127.0.0.1 <file>` 会因多一个目标而 25ms 就退出（ExitStatus 1），
    /// 所以用 `& rem <file>` 把文件参数降级成注释。裸 `ping` 是 2.05s。
    #[test]
    fn badge_stays_while_process_is_alive() {
        let vol = LocalVolume::new(std::env::temp_dir());
        let file = temp_file("alive");
        let rt = tokio::runtime::Runtime::new().unwrap();

        let open = rt
            .block_on(vol.spawn_external_open(
                &VPath(file.clone()),
                "cmd.exe /c ping -n 3 127.0.0.1 > nul & rem",
            ))
            .expect("spawn");
        let key = file.display().to_string();
        vol.external_opens().insert(key.clone(), open);

        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(
            vol.external_opens().get(&key).is_some(),
            "进程仍存活，角标不应消失"
        );

        // 收尾：等它自己结束，确认最终会消失（否则残留看门狗线程/句柄）
        let cleared = wait_until(
            || vol.external_opens().get(&key).is_none(),
            std::time::Duration::from_secs(10),
        );
        assert!(cleared, "进程结束后角标仍未消失");
        std::fs::remove_file(&file).ok();
    }

    /// 句柄泄漏判据：反复外置打开不应让本进程句柄数单调上涨。
    ///
    /// 旧实现每次 `CreateProcessW` 都漏掉 hProcess/hThread 两个句柄，
    /// 20 次就是 40 个。这里给 8 个的余量吸收噪声（测试框架自身会开句柄）。
    #[test]
    fn repeated_spawn_does_not_leak_handles() {
        let vol = LocalVolume::new(std::env::temp_dir());
        let file = temp_file("leak");
        let rt = tokio::runtime::Runtime::new().unwrap();

        // 预热：让首次的运行时/线程池开销先落定
        for _ in 0..3 {
            let _ = rt.block_on(vol.spawn_external_open(&VPath(file.clone()), "cmd.exe /c exit"));
        }
        // 等预热批次全部回收
        std::thread::sleep(std::time::Duration::from_millis(600));

        let before = handle_count();
        for _ in 0..20 {
            let open = rt
                .block_on(vol.spawn_external_open(&VPath(file.clone()), "cmd.exe /c exit"))
                .expect("spawn");
            vol.external_opens()
                .insert(format!("k{}", open.started_at_ms), open);
        }
        // 等 20 个看门狗线程全部 CloseHandle
        let settled = wait_until(
            || vol.external_opens().is_empty(),
            std::time::Duration::from_secs(15),
        );
        assert!(settled, "看门狗未全部回收条目");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let after = handle_count();

        let growth = after.saturating_sub(before);
        assert!(
            growth <= 8,
            "句柄数增长 {growth}（{before} → {after}），疑似泄漏"
        );
        std::fs::remove_file(&file).ok();
    }
}
