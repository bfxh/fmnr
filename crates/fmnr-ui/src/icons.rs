use eframe::egui;
use fmnr_core::IconKey;
use fmnr_icons::IconResolver;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 光栅化尺寸（逻辑像素）；大图标视图可再放大。
const ICON_SIZE: u32 = 32;

/// 图标缓存：IconKey → 已上传的纹理。
/// 光栅化只在首次遇到某键时发生，之后走纹理复用。
pub struct IconCache {
    resolver: IconResolver,
    textures: HashMap<String, egui::TextureHandle>,
    /// 解析失败的键，避免每帧重试。
    failed: HashMap<String, ()>,
}

impl IconCache {
    /// overrides_dir: 用户覆盖图标目录；builtin_dir: 内置集目录。
    pub fn new(overrides_dir: impl AsRef<Path>, builtin_dir: impl AsRef<Path>) -> Self {
        Self {
            resolver: IconResolver::new(overrides_dir, builtin_dir),
            textures: HashMap::new(),
            failed: HashMap::new(),
        }
    }

    /// 内置集目录：仓库内 `crates/fmnr-icons/assets`。
    pub fn builtin_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../fmnr-icons/assets")
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("crates/fmnr-icons/assets"))
    }

    fn cache_key(key: &IconKey) -> String {
        match key {
            IconKey::ByExtension(e) => format!("ext:{e}"),
            IconKey::ByName(n) => format!("name:{n}"),
            IconKey::ByHash(h) => format!("hash:{h}"),
            IconKey::Directory => "dir".into(),
            IconKey::Generic => "generic".into(),
        }
    }

    /// 取纹理；首次调用时同步光栅化（32×32 SVG，微秒级）。
    pub fn get(&mut self, ctx: &egui::Context, key: &IconKey) -> Option<egui::TextureHandle> {
        let ck = Self::cache_key(key);
        if let Some(t) = self.textures.get(&ck) {
            return Some(t.clone());
        }
        if self.failed.contains_key(&ck) {
            return None;
        }

        let svg_path = self.resolver.resolve(key)?;
        match fmnr_icons::rasterize_svg(&svg_path, ICON_SIZE) {
            Ok(rgba) => {
                let img = egui::ColorImage::from_rgba_unmultiplied(
                    [ICON_SIZE as usize, ICON_SIZE as usize],
                    &rgba,
                );
                let handle = ctx.load_texture(ck.clone(), img, egui::TextureOptions::LINEAR);
                self.textures.insert(ck, handle.clone());
                Some(handle)
            }
            Err(_) => {
                self.failed.insert(ck, ());
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内置集必须真的能被解析出来——路径写错会让整条管线静默退回 emoji。
    #[test]
    fn builtin_assets_resolve_and_rasterize() {
        let dir = IconCache::builtin_dir();
        assert!(
            dir.join("folder.svg").exists(),
            "内置图标目录解析错误：{}",
            dir.display()
        );

        let resolver = IconResolver::new(dir.join("__no_overrides__"), &dir);

        for key in [
            IconKey::Directory,
            IconKey::Generic,
            IconKey::ByExtension("rs".into()),
            IconKey::ByExtension("png".into()),
            IconKey::ByExtension("mp4".into()),
        ] {
            let path = resolver.resolve(&key).unwrap_or_else(|| panic!("未解析出 {key:?}"));
            let rgba = fmnr_icons::rasterize_svg(&path, ICON_SIZE)
                .unwrap_or_else(|e| panic!("光栅化 {key:?} 失败：{e}"));
            assert_eq!(rgba.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            // 全透明 = 画了个空白，等于没图标。
            let opaque = rgba.chunks_exact(4).any(|px| px[3] > 0);
            assert!(opaque, "{key:?} 光栅化结果全透明");
        }
    }

    /// 未知扩展名要落到通用文件图标，而不是 None。
    #[test]
    fn unknown_extension_falls_back_to_generic() {
        let dir = IconCache::builtin_dir();
        let resolver = IconResolver::new(dir.join("__no_overrides__"), &dir);
        let got = resolver.resolve(&IconKey::ByExtension("zzzznope".into()));
        assert_eq!(got, Some(dir.join("file.svg")));
    }
}
