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
        return PreviewContent::Binary {
            size: bytes.len() as u64,
        };
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

/// 预览内容的来源决策：插件先认领，没人认领才回落内置分类。
///
/// **插件渲染失败不回落**。回落会让用户看到内置预览，以为插件没生效；
/// 报失败至少让人知道是插件的问题。这与「没有插件认领」是两种不同的结果，
/// 必须区分开——这是本函数存在的全部理由。
pub fn route(
    plugins: &mut fmnr_plugins::registry::PluginSet,
    path: &Path,
    bytes: &[u8],
    truncated: bool,
) -> PreviewContent {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    match plugins.render(&ext, bytes) {
        Some(Ok(out)) => PreviewContent::Text {
            text: String::from_utf8_lossy(&out).into_owned(),
            truncated: false,
        },
        Some(Err(e)) => PreviewContent::Failed(format!("插件渲染失败：{e}")),
        None => classify(path, bytes, truncated),
    }
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

    /// 一个真干活的插件：输入转大写。
    const UPPER_PLUGIN: &str = r#"
(module
  (@custom "fmnr.plugin" "{\"name\":\"upper\",\"version\":\"1.0\",\"extensions\":[\"txt\"]}")
  (memory (export "memory") 1)
  (global $bump (mut i32) (i32.const 1024))
  (func (export "fmnr_abi_version") (result i32) i32.const 1)
  (func (export "fmnr_alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (i32.add (local.get $len) (i32.const 16))))
    (local.get $p))
  (func (export "fmnr_render") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32) (local $i i32) (local $b i32)
    (local.set $out (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (i32.add (local.get $len) (i32.const 16))))
    (block $done
      (loop $loop
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $b (i32.load8_u (i32.add (local.get $ptr) (local.get $i))))
        (if (i32.and (i32.ge_u (local.get $b) (i32.const 97))
                     (i32.le_u (local.get $b) (i32.const 122)))
          (then (local.set $b (i32.sub (local.get $b) (i32.const 32)))))
        (i32.store8 (i32.add (local.get $out) (local.get $i)) (local.get $b))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $loop)))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $len)) (i64.const 32))
      (i64.extend_i32_u (local.get $out)))))
"#;

    /// 插件声明处理 txt：预览必须走插件，而不是内置文本预览。
    #[test]
    fn plugin_claims_extension_and_takes_over() {
        let dir = std::env::temp_dir().join(format!("fmnr-prev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("u.wasm"), wat::parse_str(UPPER_PLUGIN).unwrap()).unwrap();

        let (mut set, warns) = fmnr_plugins::registry::PluginSet::load_dir(&dir);
        assert!(warns.is_empty(), "好插件不该有警告：{warns:?}");

        let c = route(&mut set, Path::new("a.txt"), b"hello", false);
        match c {
            PreviewContent::Text { text, .. } => {
                assert_eq!(text, "HELLO", "插件应接管 txt 预览并做变换");
            }
            other => panic!("应走插件：{:?}", std::mem::discriminant(&other)),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 插件没认领的扩展名：回落内置分类（不是报错）。
    #[test]
    fn unclaimed_extension_falls_back_to_builtin() {
        let mut set = fmnr_plugins::registry::PluginSet::new();
        let c = route(&mut set, Path::new("a.md"), b"# title", false);
        assert!(is_text(&c), "无插件时应回落内置文本预览");
    }

    /// 关键区分：插件认领了但渲染失败，必须报失败而**不是**回落内置预览。
    /// 回落会让用户以为插件没生效。
    #[test]
    fn plugin_failure_is_reported_not_silently_fallen_back() {
        let dir = std::env::temp_dir().join(format!("fmnr-prevfail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 认领 txt，但渲染时死循环 ⇒ 燃料耗尽。
        let bad = r#"
(module
  (@custom "fmnr.plugin" "{\"name\":\"spin\",\"version\":\"1.0\",\"extensions\":[\"txt\"]}")
  (memory (export "memory") 1)
  (func (export "fmnr_abi_version") (result i32) i32.const 1)
  (func (export "fmnr_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "fmnr_render") (param i32) (param i32) (result i64)
    (loop $f (br $f)) i64.const 0))
"#;
        std::fs::write(dir.join("bad.wasm"), wat::parse_str(bad).unwrap()).unwrap();
        let (mut set, _) = fmnr_plugins::registry::PluginSet::load_dir(&dir);

        let c = route(&mut set, Path::new("a.txt"), b"hello", false);
        match c {
            PreviewContent::Failed(msg) => {
                assert!(msg.contains("插件"), "失败信息应点明是插件的问题：{msg}");
            }
            PreviewContent::Text { .. } => {
                panic!("插件失败时不能静默回落成内置文本预览");
            }
            other => panic!("应报失败：{:?}", std::mem::discriminant(&other)),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
