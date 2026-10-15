use fmnr_core::IconKey;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 图标解析器：hash → 文件名 → 扩展名 → 内置集 优先级链。
/// 自管线渲染，不调用系统 SHGetFileInfo。
pub struct IconResolver {
    /// 覆盖表：键 → SVG 文件路径。键形如 "ext:rs" / "name:Cargo.toml" / "hash:abc123"
    overrides: HashMap<String, PathBuf>,
    /// 内置图标集目录（按扩展名命名 .svg）
    builtin_dir: PathBuf,
    /// 扩展名 → 内置分类（代码/图片/视频/压缩/文档/系统）
    category: HashMap<String, &'static str>,
}

impl IconResolver {
    /// overrides_dir: 用户覆盖图标目录；builtin_dir: 内置集目录。
    pub fn new(overrides_dir: impl AsRef<Path>, builtin_dir: impl AsRef<Path>) -> Self {
        let mut overrides = HashMap::new();
        scan_overrides(overrides_dir.as_ref(), &mut overrides);

        Self {
            overrides,
            builtin_dir: builtin_dir.as_ref().to_path_buf(),
            category: default_categories(),
        }
    }

    /// 解析图标键 → SVG 源文件路径（若存在）。
    pub fn resolve(&self, key: &IconKey) -> Option<PathBuf> {
        match key {
            IconKey::ByHash(h) => self
                .overrides
                .get(&format!("hash:{h}"))
                .cloned()
                .or_else(|| self.builtin_for(None)),
            IconKey::ByName(n) => self
                .overrides
                .get(&format!("name:{n}"))
                .cloned()
                .or_else(|| self.builtin_for(None)),
            IconKey::ByExtension(ext) => self
                .overrides
                .get(&format!("ext:{ext}"))
                .cloned()
                .or_else(|| self.builtin_for(Some(ext))),
            IconKey::Directory => self
                .builtin_dir
                .join("folder.svg")
                .exists()
                .then(|| self.builtin_dir.join("folder.svg")),
            IconKey::Generic => self.builtin_for(None),
        }
    }

    fn builtin_for(&self, ext: Option<&str>) -> Option<PathBuf> {
        if let Some(ext) = ext {
            let direct = self.builtin_dir.join(format!("{ext}.svg"));
            if direct.exists() {
                return Some(direct);
            }
            if let Some(cat) = self.category.get(ext) {
                let by_cat = self.builtin_dir.join(format!("{cat}.svg"));
                if by_cat.exists() {
                    return Some(by_cat);
                }
            }
        }
        let fallback = self.builtin_dir.join("file.svg");
        fallback.exists().then_some(fallback)
    }
}

fn scan_overrides(dir: &Path, out: &mut HashMap<String, PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for item in rd.flatten() {
            let path = item.path();
            if let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().to_string()) {
                // 约定：文件名即键，如 "ext:rs.svg" / "name:Cargo.toml.svg" / "hash:abc.svg"
                if let Some((kind, key)) = stem.split_once(':') {
                    out.insert(format!("{kind}:{key}"), path);
                }
            }
        }
    }
}

fn default_categories() -> HashMap<String, &'static str> {
    let mut m = HashMap::new();
    for e in ["rs", "py", "js", "ts", "tsx", "jsx", "go", "c", "cpp", "h", "java", "rb", "sh"] {
        m.insert(e.to_string(), "code");
    }
    for e in ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "heic"] {
        m.insert(e.to_string(), "image");
    }
    for e in ["mp4", "mkv", "avi", "mov", "webm", "mp3", "flac", "wav", "ogg"] {
        m.insert(e.to_string(), "media");
    }
    for e in ["zip", "tar", "gz", "7z", "rar", "bz2", "xz", "zst"] {
        m.insert(e.to_string(), "archive");
    }
    for e in ["txt", "md", "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "json", "toml", "yaml", "yml", "xml", "csv"] {
        m.insert(e.to_string(), "doc");
    }
    for e in ["exe", "dll", "sys", "msi"] {
        m.insert(e.to_string(), "app");
    }
    m
}

/// 把 SVG 光栅化为 RGBA 位图（在独立线程调用，勿阻塞 UI）。
pub fn rasterize_svg(svg_path: &Path, size: u32) -> Result<Vec<u8>, String> {
    let data = std::fs::read(svg_path).map_err(|e| e.to_string())?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .map_err(|e| format!("usvg: {e}"))?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or("pixmap alloc")?;
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap.data().to_vec())
}
