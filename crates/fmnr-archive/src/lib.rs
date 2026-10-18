use fmnr_core::{
    Attr, ConflictPolicy, Entry, Error, ExternalOpen, ExternalOpens, IconKey, ListOpts, OpReport,
    Progress, Result, SortOrder, VPath, Volume, VolumeId, VolumeKind,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 路径解析错误。Invalid 留给外包一层包装，未知后缀落到此。
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("不是支持的归档格式（zip / tar / tar.gz / 7z）: {0}")]
    UnsupportedFormat(String),
    #[error("路径不在归档内: {0}")]
    OutsideArchive(String),
    #[error("归档内路径非法: {0}")]
    Invalid(String),
}

/// 归档卷：把 zip / tar / tar.gz / 7z 以只读方式挂载为 Volume。
///
/// 进卷时把**全量条目索引**建好（zip 读中央目录；tar/tar.gz 顺序扫头块，gz 要整条
/// inflate），成员内容按需才读——列目录不解压、不占内存，但建索引这一步不是懒的。
/// 路径形如 `archive:/abs/path/a.zip/sub/dir`。
pub struct ArchiveVolume {
    id: VolumeId,
    archive_path: PathBuf,
    entries: BTreeMap<String, Entry>,
    external_opens: Arc<ExternalOpens>,
}

/// 把 `archive:/abs/path/a.zip/sub/dir\file.txt` 拆成
/// `(PathBuf("/abs/path/a.zip"), "/sub/dir/file.txt")`。
///
/// 内层路径统一用 `/` 分隔（zip/tar 的标准），调用方传 Windows 反斜杠
/// 也接受——用户在地址栏里手打的路径不会因为分隔符被拒。
pub fn split_path(path: &Path) -> std::result::Result<(PathBuf, String), PathError> {
    let s = path.to_string_lossy();
    let rest = s
        .strip_prefix("archive:/")
        .or_else(|| s.strip_prefix("archive:"))
        .ok_or_else(|| PathError::Invalid(format!("缺少 archive: 前缀：{s}")))?;
    let rest = rest.replace('\\', "/");

    // 找归档文件本身：zip 后缀必须出现，且优先取**最外层**的 .zip。
    // 注意 Windows 盘符 `C:` 里有冒号，不能拿冒号切分。
    let ext_pos = rest
        .find(".zip")
        .ok_or_else(|| PathError::UnsupportedFormat(s.to_string()))?;
    let end = ext_pos + ".zip".len();
    let (archive_file, inner) = rest.split_at(end);
    let inner = inner.trim_start_matches('/').to_string();

    Ok((PathBuf::from(archive_file), inner))
}

impl ArchiveVolume {
    /// 打开一个归档文件并建立目录索引。
    pub fn open(archive_path: impl Into<PathBuf>) -> Result<Self> {
        let archive_path = archive_path.into();
        let entries = index(&archive_path)?;

        Ok(Self {
            id: VolumeId(format!("archive:{}", archive_path.display())),
            archive_path,
            entries,
            external_opens: Arc::new(ExternalOpens::new()),
        })
    }

    /// 卷内路径的**规范化**key：以 `/` 分隔、无前导斜杠。
    ///
    /// 三种入法都要认（少一种就有一条入口暗断）：
    /// - 纯卷内相对路径 `sub/dir`（UI 面包屑就是这么传的）；
    /// - 带卷前缀的完整路径 `archive:<archive_path>/sub/dir`（MCP 客户端、
    ///   CLI 卷路由都按卷 id 前缀传）；
    /// - `archive:` 本身 = 卷根。
    ///
    /// 老实现只认 `archive:/` 前缀，在 **Windows 盘符**下剥不掉
    /// （`archive:D:/x.zip` 不以 `archive:/` 开头），整个归档路径漏进 key
    /// ⇒ 必挂 NotFound（实测 `fmnr ls archive:D:\x.zip` 报 not found 即此）。
    /// 用大小写敏感的 `strip_prefix`：入参前缀来自卷 id，与 archive_path
    /// 出自同一串必然一致；折叠大小写再切片反而有长度变化的 panic 风险。
    fn key_of(&self, path: &VPath) -> String {
        let raw = path.0.to_string_lossy().replace('\\', "/");
        let Some(rest) = raw.strip_prefix("archive:") else {
            return raw.trim_start_matches('/').to_string();
        };
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() {
            return String::new();
        }
        let ap = self
            .archive_path
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
            .to_string();
        if let Some(tail) = rest.strip_prefix(&ap) {
            if tail.is_empty() {
                return String::new();
            }
            // 归档路径后必须紧跟子路径分隔；`x.zip2` 这种同前缀名不算。
            if let Some(sub) = tail.strip_prefix('/') {
                return sub.to_string();
            }
        }
        // 其余情况按纯卷内相对路径（与旧行为一致，不把存量契约测红）。
        rest.to_string()
    }
}

/// 扫描归档，建立 `key -> Entry` 索引。
///
/// 目录**显式**加进来（zip 里常见显式目录项，tar 则完全没有），
/// 这样列父目录时才能把子目录显示出来。
fn index(path: &Path) -> Result<BTreeMap<String, Entry>> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let mut map = BTreeMap::new();
    match ext.as_str() {
        "zip" => index_zip(path, &mut map)?,
        "tar" | "tgz" | "gz" => index_tar(path, &mut map)?,
        "7z" => index_7z(path, &mut map)?,
        other => {
            return Err(Error::Unsupported(format!(
                "归档格式 {other} 暂不支持（zip / tar / tar.gz / 7z）"
            )))
        }
    }
    ensure_parent_dirs(&mut map);
    Ok(map)
}

/// 7z：只读挂载，编码面**故意只开一档**。
///
/// `sevenz-rust2` 用 `default-features = false` 引入，所以这里能解的是
/// COPY / LZMA / LZMA2 / BCJ / DELTA（= 7z 的默认与主流编码）。
/// `bzip2` / `PPMd` / `deflate` / `lz4` / `zstd` / AES 各自的可选依赖**没开**：
/// 一是这一版没有"输密码"的入口（加密件照样读不出来，开了解密特征等于白引依赖），
/// 二是 `bzip2` 那类是 C 绑定。撞上时会**明确报错**，不会给出半截或错内容。
/// 🔴 有门的只有 AES 那一条（`encrypted_7z_fails_loudly_instead_of_mounting_empty`，
/// 拿 7-Zip 26.00 真写的加密件）：本机 `7zr` 的 codec 表里根本没有 BZip2/PPMd，
/// 也找不到全量 `7z.exe`，**造不出那几类的真件**，所以其余编码的报错通路只是
/// 同一段代码的推论，没钉成门。
fn index_7z(path: &Path, map: &mut BTreeMap<String, Entry>) -> Result<()> {
    let ar =
        sevenz_rust2::Archive::open(path).map_err(|e| Error::Other(format!("7z 打开失败：{e}")))?;
    for e in &ar.files {
        // 反目项（anti-item，增量更新里表示"这里被删了"）不是文件，列出来会变成
        // 一个点不开的幽灵条目。
        if e.is_anti_item {
            continue;
        }
        // 分隔符与查询侧（`key_of`）保持同一套规范化。实测 7z 头表用的是 `/`
        // （本机 7-Zip 26.00 写的件 + 两个现成件共 3,358 条，0 条含反斜杠），
        // 🔴 所以这一句**没有门能钉**：造不出反斜杠形态的真件。留着是因为查询侧
        // 本来就做同样的转换，两边口径不一致会比多做一次更糟。
        let name = e.name.replace('\\', "/");
        let key = name.trim_end_matches('/').to_string();
        if key.is_empty() {
            continue;
        }
        let is_dir = e.is_directory;
        let mut attrs = Vec::new();
        if is_unix_hidden(&key) || windows_hidden(e.windows_attributes, e.has_windows_attributes) {
            attrs.push(Attr::Hidden);
        }
        map.insert(
            key.clone(),
            Entry {
                name: key.rsplit('/').next().unwrap_or(&key).to_string(),
                is_dir,
                size: if is_dir { None } else { Some(e.size) },
                modified: nt_mtime(e.has_last_modified_date, e.last_modified_date),
                icon_key: icon_for(&key, is_dir),
                attrs,
            },
        );
    }
    Ok(())
}

/// 7z 的 Windows 属性位（`FILE_ATTRIBUTE_HIDDEN = 0x2`）；件里没带属性就不判。
fn windows_hidden(attrs: u32, present: bool) -> bool {
    present && (attrs & 0x2) != 0
}

/// NT 时间（1601 年起、100 ns 一格）→ `SystemTime`。
///
/// 不用 crate 给的 `From<NtTime> for SystemTime`：它先算
/// `UNIX_EPOCH - 369 年`，而 `SystemTime` 的减法在"结果早于平台零"时**会 panic**——
/// 一个日期字段写坏的 7z 就能把列目录打崩（本机实测：走 crate 的 `From` 时
/// `raw = u64::MAX` **panic** 在 `time.rs:98`；而 `raw = 0/1` 那种 1970 年前的反倒不崩）。
/// 自己做减法就得自己收两端：1970 之前、或晚于「epoch + 400 年」的（明显是坏头部）
/// 一律 `None` —— 列出来就是"没有修改时间"，不装、不崩，也不会显示一个荒谬年份。
fn nt_mtime(present: bool, t: sevenz_rust2::NtTime) -> Option<std::time::SystemTime> {
    use std::time::{Duration, SystemTime};
    if !present {
        return None;
    }
    let raw = u64::from(t);
    let epoch = u64::from(sevenz_rust2::NtTime::UNIX_EPOCH);
    if raw < epoch {
        return None;
    }
    /// 100 ns 一格 × 400 年：真实文件日期远在这之前，越过即坏数据。
    const MAX_TICKS: u64 = 400 * 366 * 86_400 * 10_000_000;
    let d = raw - epoch;
    if d > MAX_TICKS {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::new(d / 10_000_000, ((d % 10_000_000) * 100) as u32))
}

fn index_zip(path: &Path, map: &mut BTreeMap<String, Entry>) -> Result<()> {
    let file = std::fs::File::open(path).map_err(Error::Io)?;
    let reader = std::io::BufReader::new(file);
    // 只读中央目录，不解压内容——大 zip 也不该全量枚举内容。
    let mut zip =
        zip::ZipArchive::new(reader).map_err(|e| Error::Other(format!("zip 打开失败：{e}")))?;

    for i in 0..zip.len() {
        let entry = zip
            .by_index_raw(i)
            .map_err(|e| Error::Other(format!("zip 条目 {i} 读取失败：{e}")))?;
        // zip 9 的 name() 返回 Result<Cow<str>>：编码坏了就是错误，不能当空文件名继续。
        let name = entry
            .name()
            .map_err(|e| Error::Other(format!("zip 条目 {i} 名字解码失败：{e}")))?
            .replace('\\', "/");
        let key = name.trim_end_matches('/').to_string();
        let is_dir = entry.is_dir() || key.is_empty();

        let mut attrs = Vec::new();
        if is_unix_hidden(&key) {
            attrs.push(Attr::Hidden);
        }

        let e = Entry {
            name: key.rsplit('/').next().unwrap_or(&key).to_string(),
            is_dir,
            size: if is_dir { None } else { Some(entry.size()) },
            modified: None,
            icon_key: icon_for(&key, is_dir),
            attrs,
        };
        map.insert(key, e);
    }
    Ok(())
}

/// tar 条目的两种读法：**要不要先过一层 gzip**。
///
/// `.tgz`/`.tar.gz` 很常见。早先这里只按扩展名分流、把 `.gz` 也喂给纯 tar
/// 解析器，还写了句「按内容探测」的注释——代码其实没探测。现在真探测。
enum TarReader {
    Plain(std::io::BufReader<std::fs::File>),
    // GzDecoder 内联进 enum 会让两个变体大小差 7 倍（328 vs 48 字节），
    // 每次 open 都按最大值分配；装箱后 enum 只有一个指针宽。
    Gz(Box<flate2::read::GzDecoder<std::io::BufReader<std::fs::File>>>),
}

impl std::io::Read for TarReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            TarReader::Plain(r) => r.read(buf),
            TarReader::Gz(r) => r.read(buf),
        }
    }
}

/// 按**内容魔数**（`1f 8b` = gzip）而不是扩展名决定解不解压。
///
/// 扩展名会骗人（`.tar` 实为 gzip、`.gz` 实为纯 tar 都存在），魔数不会。
///
/// 必须用 `fill_buf`（**窥探**）而不是 `read`：早先用 `read` 把前两字节
/// 吃掉才交给 tar，于是每个条目都从偏移 2 开始解析，报 checksum mismatch
/// / unexpected EOF——整份归档全废。
fn open_tar(path: &Path) -> Result<TarReader> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).map_err(Error::Io)?;
    let mut probe = std::io::BufReader::new(file);
    let gz = probe
        .fill_buf()
        .map_err(Error::Io)?
        .starts_with(&[0x1f, 0x8b][..]);
    if gz {
        // 魔数仍留在 BufReader 缓冲区里，GzDecoder 会从头读到它。
        Ok(TarReader::Gz(Box::new(flate2::read::GzDecoder::new(probe))))
    } else {
        Ok(TarReader::Plain(probe))
    }
}

fn index_tar(path: &Path, map: &mut BTreeMap<String, Entry>) -> Result<()> {
    let mut archive = tar::Archive::new(open_tar(path)?);
    let entries = archive
        .entries()
        .map_err(|e| Error::Other(format!("tar 打开失败：{e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::Other(format!("tar 条目读取失败：{e}")))?;
        let header = entry.header();
        let name = header
            .path()
            .map_err(|e| Error::Other(format!("{e}")))?
            .to_string_lossy()
            .replace('\\', "/");
        let key = name.trim_end_matches('/').to_string();
        let is_dir = header.entry_type().is_dir();

        let mut attrs = Vec::new();
        if is_unix_hidden(&key) {
            attrs.push(Attr::Hidden);
        }

        let e = Entry {
            name: key.rsplit('/').next().unwrap_or(&key).to_string(),
            is_dir,
            size: if is_dir {
                None
            } else {
                // 尺寸读不出来（损坏的八进制字段、非 GNU 的 sparse 头）要报"不知道"，
                // 不能报 0 —— 0 在界面上是一个确凿的空文件。
                // 同一条口径见 drives.rs：读不到就 Err/None，别用 0 顶。
                header.size().ok()
            },
            modified: header
                .mtime()
                .ok()
                .map(|m| std::time::UNIX_EPOCH + std::time::Duration::from_secs(m)),
            icon_key: icon_for(&key, is_dir),
            attrs,
        };
        map.insert(key, e);
    }
    Ok(())
}

/// tar 没有显式目录项：`a/b.txt` 存在但 `a` 不在 map 里时，
/// 列父目录会漏掉 `a`。这里补全所有中间目录。
fn ensure_parent_dirs(map: &mut BTreeMap<String, Entry>) {
    let keys: Vec<String> = map.keys().cloned().collect();
    for key in keys {
        let parts: Vec<&str> = key.split('/').collect();
        // 只补**真父级**：最后一段是条目自己，不要给自己补目录。
        for i in 1..parts.len() {
            let parent = parts[..i].join("/");
            if parent.is_empty() {
                continue;
            }
            map.entry(parent.clone()).or_insert_with(|| Entry {
                name: parent.rsplit('/').next().unwrap_or(&parent).to_string(),
                is_dir: true,
                size: None,
                modified: None,
                icon_key: IconKey::Directory,
                attrs: Vec::new(),
            });
        }
    }
}

fn is_unix_hidden(key: &str) -> bool {
    key.split('/').any(|seg| seg.starts_with('.'))
}

fn icon_for(key: &str, is_dir: bool) -> IconKey {
    if is_dir {
        return IconKey::Directory;
    }
    match Path::new(key)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
    {
        Some(ext) if !ext.is_empty() => IconKey::ByExtension(ext),
        _ => IconKey::Generic,
    }
}

/// 直接子项：key 的父目录恰好是 `dir`。
///
/// 返回 Vec 而不是 `impl Iterator`：过滤器要捕获 `prefix` 这个局部值，
/// 写成 RPIT 会把它的匿名生命周期带进返回类型而编不过（E0700）。
fn direct_children<'a>(
    entries: &'a BTreeMap<String, Entry>,
    dir: &str,
) -> Vec<(&'a String, &'a Entry)> {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    entries
        .iter()
        .filter(|(k, _)| {
            if k.as_str() == dir {
                return false;
            }
            let Some(rest) = k.strip_prefix(prefix.as_str()) else {
                return false;
            };
            // rest 里不能再有 '/'，否则是孙级
            !rest.is_empty() && !rest.contains('/')
        })
        .collect()
}

#[async_trait::async_trait]
impl Volume for ArchiveVolume {
    fn id(&self) -> VolumeId {
        self.id.clone()
    }

    fn kind(&self) -> VolumeKind {
        VolumeKind::Archive
    }

    async fn list(&self, path: &VPath, opts: &ListOpts) -> Result<Vec<Entry>> {
        let dir = self.key_of(path);
        // 不存在（且不是根）→ NotFound，与 LocalVolume 的口径一致
        if !dir.is_empty() && !self.entries.contains_key(&dir) {
            return Err(Error::NotFound(format!(
                "{}!{}",
                self.archive_path.display(),
                dir
            )));
        }
        if !dir.is_empty() && !self.entries[&dir].is_dir {
            return Err(Error::Unsupported(format!("{} 不是目录", dir)));
        }

        let mut out: Vec<Entry> = direct_children(&self.entries, &dir)
            .into_iter()
            .filter(|(_, e)| opts.show_hidden || !e.attrs.contains(&Attr::Hidden))
            .map(|(_, e)| e.clone())
            .collect();
        sort_entries(&mut out, opts.sort);
        Ok(out)
    }

    async fn read_stream(
        &self,
        path: &VPath,
    ) -> Result<Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>> {
        let key = self.key_of(path);
        let Some(entry) = self.entries.get(&key) else {
            return Err(Error::NotFound(format!(
                "{}!{}",
                self.archive_path.display(),
                key
            )));
        };
        if entry.is_dir {
            return Err(Error::Unsupported(format!("{} 是目录，不能读取", key)));
        }

        let archive_path = self.archive_path.clone();
        let key = key.clone();
        let ext = archive_path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            match ext.as_str() {
                "zip" => read_zip_entry(&archive_path, &key),
                "tar" | "tgz" | "gz" => read_tar_entry(&archive_path, &key),
                "7z" => read_7z_entry(&archive_path, &key),
                other => Err(Error::Other(format!("不支持的归档格式：{other}"))),
            }
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))??;

        // 上限：预览面板已有读取上限，这里不再叠加一层；
        // 但整文件读进内存对超大成员仍是风险，靠调用方（预览）控制。
        let chunk = bytes;
        let stream = futures::stream::iter(std::iter::once(Ok(chunk)));
        Ok(Box::new(stream)
            as Box<
                dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin,
            >)
    }

    async fn write(&self, path: &VPath, _data: &[u8]) -> Result<()> {
        Err(Error::Unsupported(format!(
            "归档是只读的：{}",
            path.0.display()
        )))
    }

    fn writable(&self) -> bool {
        false
    }

    fn native_path(&self, path: &VPath) -> Option<PathBuf> {
        // 归档内文件没有原生路径：要交给外部程序，必须先解到临时目录。
        // 这里返回 None，让上层走「先拉取再打开」的路径（见 enter/open 逻辑）。
        let _ = path;
        None
    }

    fn external_opens(&self) -> &ExternalOpens {
        &self.external_opens
    }

    async fn spawn_external_open(&self, file_path: &VPath, _program: &str) -> Result<ExternalOpen> {
        Err(Error::Unsupported(format!(
            "归档卷不能直接打开外部程序：{}",
            file_path.0.display()
        )))
    }

    async fn copy(
        &self,
        sources: &[VPath],
        dest_dir: &VPath,
        _policy: ConflictPolicy,
        _progress: Arc<Progress>,
    ) -> Result<OpReport> {
        Err(Error::Unsupported(format!(
            "归档卷是只读的：{} → {}",
            sources.len(),
            dest_dir.0.display()
        )))
    }

    async fn move_entries(
        &self,
        sources: &[VPath],
        dest_dir: &VPath,
        _policy: ConflictPolicy,
        _progress: Arc<Progress>,
    ) -> Result<OpReport> {
        Err(Error::Unsupported(format!(
            "归档卷是只读的：{} → {}",
            sources.len(),
            dest_dir.0.display()
        )))
    }

    async fn delete(&self, targets: &[VPath], _progress: Arc<Progress>) -> Result<OpReport> {
        Err(Error::Unsupported(format!(
            "归档卷是只读的：{}",
            targets.len()
        )))
    }

    async fn rename(&self, from: &VPath, _to: &VPath) -> Result<()> {
        Err(Error::Unsupported(format!(
            "归档卷是只读的：{}",
            from.0.display()
        )))
    }

    async fn create_dir(&self, path: &VPath) -> Result<()> {
        Err(Error::Unsupported(format!(
            "归档卷是只读的：{}",
            path.0.display()
        )))
    }

    async fn search(&self, root: &VPath, needle: String, limit: usize) -> Result<Vec<VPath>> {
        let dir = self.key_of(root);
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut out = Vec::new();
        for (key, e) in &self.entries {
            if e.is_dir {
                continue;
            }
            if !key.starts_with(&prefix) {
                continue;
            }
            let name = key.rsplit('/').next().unwrap_or(key);
            if fmnr_core::search::name_matches(name, &needle) {
                out.push(VPath(PathBuf::from(format!(
                    "archive:{}!/{}",
                    self.archive_path.display(),
                    key
                ))));
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }
}

fn read_zip_entry(archive_path: &Path, key: &str) -> Result<Vec<u8>> {
    let file = std::fs::File::open(archive_path).map_err(Error::Io)?;
    let reader = std::io::BufReader::new(file);
    let mut zip =
        zip::ZipArchive::new(reader).map_err(|e| Error::Other(format!("zip 打开失败：{e}")))?;
    let mut entry = zip
        .by_name(key)
        .map_err(|_| Error::NotFound(format!("{}!{}", archive_path.display(), key)))?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    std::io::copy(&mut entry, &mut buf).map_err(Error::Io)?;
    Ok(buf)
}

fn read_tar_entry(archive_path: &Path, key: &str) -> Result<Vec<u8>> {
    let mut archive = tar::Archive::new(open_tar(archive_path)?);
    let entries = archive
        .entries()
        .map_err(|e| Error::Other(format!("tar 打开失败：{e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| Error::Other(format!("{e}")))?;
        let name = entry
            .path()
            .map_err(|e| Error::Other(format!("{e}")))?
            .to_string_lossy()
            .replace('\\', "/");
        if name.trim_end_matches('/') == key {
            let mut buf = Vec::new();
            std::io::copy(&mut entry, &mut buf).map_err(Error::Io)?;
            return Ok(buf);
        }
    }
    Err(Error::NotFound(format!(
        "{}!{}",
        archive_path.display(),
        key
    )))
}

/// 读 7z 里的一个成员。
///
/// 直接拿规范化后的 key 交给 `read_file`。这里**实测过**才这么写的：
/// `read_file` 按头表里的原始名**精确匹配**，不做任何分隔符转换
/// （`dir\deep.bin` 查 `dir/deep.bin` 报 FileNotFound，反过来也一样），
/// 而 7z 头表里的分隔符实测就是 `/`——本机 7-Zip 26.00 写的件、
/// 以及两个别人打包的现成件（91 条 + 3,156 条，共 3,358 条）里**一条反斜杠都没有**。
/// 所以不再为了"可能存在反斜杠形态"在每次读取前重解析一遍头表：
/// 那等于每读一个成员就把整个索引重建一次。
/// 若真遇到那种件，列目录仍走 `index_7z` 的规范化，读取会明确报错，
/// 不会给出空内容或错内容。
///
/// 「成员不存在」在这里够不到：`read_stream` 先查头表索引，缺的那条在进这里之前
/// 就返回 `NotFound` 了（实测：把下面的错法改成"不认 FileNotFound"，那条门照样绿）。
/// 所以这里只把库的错原样带出去，不装出一副"我也能判不存在"的样子。
fn read_7z_entry(archive_path: &Path, key: &str) -> Result<Vec<u8>> {
    let mut reader =
        sevenz_rust2::ArchiveReader::open(archive_path, sevenz_rust2::Password::empty())
            .map_err(|e| Error::Other(format!("7z 打开失败：{e}")))?;
    // 未开启的编码（bzip2 / PPMd / deflate / lz4 / zstd / AES）在这里由库明确报错，
    // 原样带出去——绝不返回一个"看着像成功"的空内容。
    reader
        .read_file(key)
        .map_err(|e| Error::Other(format!("7z 读取 {key} 失败：{e}")))
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
                // 与 LocalVolume 同口径：按**扩展名**分组，不是按名字。
                // 两条实现排序不同会让「同一个卷换个视图就变序」这种错觉无处可查。
                .then_with(|| a.extension().cmp(&b.extension()))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NT 时间的换算必须**在任何输入下都不 panic**，且坏数据落到"没有修改时间"。
    ///
    /// 这条不是凭空写的防御：`sevenz-rust2` 自带的 `From<NtTime> for SystemTime`
    /// 在本机实测对 `raw = u64::MAX` 直接 panic（`time.rs:98`）。头表里的日期是文件
    /// 给的，一个写坏的 7z 不该把列目录打崩。
    #[test]
    fn nt_time_conversion_never_panics_and_bounds_garbage() {
        // 1970 之前（含 0 与 1 tick）：None，不是"1601 年"这种看着像真日期的东西
        for raw in [0u64, 1, 1_000_000] {
            assert_eq!(
                nt_mtime(true, sevenz_rust2::NtTime::from(raw)),
                None,
                "raw={raw}"
            );
        }
        // 荒谬的大值（含 u64::MAX）：None 而不是 panic
        for raw in [u64::MAX, u64::MAX - 1, 1u64 << 62] {
            assert_eq!(
                nt_mtime(true, sevenz_rust2::NtTime::from(raw)),
                None,
                "raw={raw}"
            );
        }
        // 没有日期字段：None
        assert_eq!(nt_mtime(false, sevenz_rust2::NtTime::from(0)), None);
        // 合法日期要能读出来。这里喂的是**自己算的** ticks（不是从夹具抄的秒数）：
        // 钉的是"NT→SystemTime 的换算本身不错"，夹具那条日期由集成门
        // `modified.is_some()` 管。年份只断言区间——7-Zip 存本地时间还是 UTC 我不赌。
        let secs = 1_792_394_990u64; // 2026-10-19 07:29:50 UTC
        let ticks = u64::from(sevenz_rust2::NtTime::UNIX_EPOCH) + secs * 10_000_000;
        let got = nt_mtime(true, sevenz_rust2::NtTime::from(ticks));
        assert!(got.is_some(), "合法日期不该被判成坏数据");
        let since = got.unwrap().duration_since(std::time::UNIX_EPOCH).unwrap();
        assert_eq!(since.as_secs(), secs);
        let year = 1970 + since.as_secs() / (365 * 86_400);
        assert!((2020..2100).contains(&year), "年份看着不像话：{year}");
    }

    /// Windows 属性位只在件里真带了属性时才参与判断。
    #[test]
    fn windows_hidden_attribute_is_used_only_when_present() {
        assert!(windows_hidden(0x2, true), "FILE_ATTRIBUTE_HIDDEN");
        assert!(!windows_hidden(0x2, false), "没带属性表就不该判隐藏");
        assert!(!windows_hidden(0x10, true), "只读位不是隐藏位");
    }
}
