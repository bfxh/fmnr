use eframe::egui;
use std::path::{Path, PathBuf};

/// 文本预览读取上限（1 MiB），超过则截断。
pub const TEXT_CAP: usize = 1 << 20;
/// 图片预览读取上限（8 MiB）。
pub const IMAGE_CAP: usize = 8 << 20;

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg"];

#[derive(Clone)]
pub enum PreviewContent {
    Text { text: String, truncated: bool },
    Image(egui::ColorImage),
    Binary { size: u64 },
    Failed(String),
}

pub struct Preview {
    pub path: PathBuf,
    pub content: PreviewContent,
    pub texture: Option<egui::TextureHandle>,
}

/// 按扩展名决定读取上限。
pub fn cap_for(path: &Path) -> usize {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        IMAGE_CAP
    } else {
        TEXT_CAP
    }
}

/// 用读到的字节构造预览内容（纯函数，便于测试）。
pub fn classify(path: &Path, bytes: &[u8], truncated: bool) -> PreviewContent {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if IMAGE_EXTS.contains(&ext.as_str()) {
        return if ext == "svg" {
            match egui_extras::image::load_svg_bytes(bytes) {
                Ok(img) => PreviewContent::Image(img),
                Err(e) => PreviewContent::Failed(format!("SVG 解码失败：{e}")),
            }
        } else {
            match egui_extras::image::load_image_bytes(bytes) {
                Ok(img) => PreviewContent::Image(img),
                Err(e) => PreviewContent::Failed(format!("图片解码失败：{e}")),
            }
        };
    }

    // 文本：前 8 KiB 含 NUL 视为二进制。
    let probe = &bytes[..bytes.len().min(8192)];
    if probe.contains(&0) {
        return PreviewContent::Binary { size: bytes.len() as u64 };
    }

    let mut text = String::from_utf8_lossy(bytes).into_owned();
    // 截断到字符边界。
    if truncated && text.len() > TEXT_CAP {
        let mut end = TEXT_CAP;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    PreviewContent::Text { text, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_text(c: &PreviewContent) -> bool {
        matches!(c, PreviewContent::Text { .. })
    }

    #[test]
    fn text_extension_without_nul_is_text() {
        let c = classify(Path::new("a.rs"), b"fn main() {}", false);
        assert!(is_text(&c));
    }

    #[test]
    fn nul_byte_probes_as_binary() {
        let c = classify(Path::new("a.txt"), b"abc\0def", false);
        assert!(matches!(c, PreviewContent::Binary { .. }));
    }

    #[test]
    fn png_garbage_is_decode_failure_not_text() {
        let c = classify(Path::new("a.png"), b"not a real png", false);
        assert!(matches!(c, PreviewContent::Failed(_)));
    }

    #[test]
    fn text_beyond_cap_truncates_at_char_boundary() {
        // 3 字节汉字 × 超过 1 MiB，截断不得 panic。
        let mut bytes: Vec<u8> = Vec::new();
        while bytes.len() < TEXT_CAP + 64 {
            bytes.extend_from_slice("汉".as_bytes());
        }
        let c = classify(Path::new("a.txt"), &bytes, true);
        match c {
            PreviewContent::Text { text, truncated } => {
                assert!(truncated);
                assert!(text.len() <= TEXT_CAP);
                assert!(text.is_char_boundary(text.len()));
            }
            other => panic!("expected text, got {:?}", std::mem::discriminant(&other)),
        }
    }
}
