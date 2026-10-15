//! 打开决策表（DESIGN §4.7.3）：UI 与 MCP 共用同一份判定。
//!
//! 放在内核而不是 UI 里，是因为「双击走预览还是走外部程序」必须对
//! 两个客户端给出**同一个答案**——否则 MCP 客户端看到的行为会和用户看到的不一致。

use crate::VPath;
use std::path::Path;

/// 双击/`open` 一个文件时的路由结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenRoute {
    /// 内置预览面板渲染（M3 统一预览）
    Preview,
    /// 交给外部程序打开；携带要启动的程序（可能含参数）
    External(String),
}

/// 内置可预览的扩展名集合。
///
/// 音视频**刻意不在内**：不内嵌 FFmpeg（DESIGN §4.7.4），走外置 + 角标。
pub const PREVIEWABLE_EXTENSIONS: &[&str] = &[
    // 文本 / 代码 / 配置
    "txt", "md", "rs", "py", "js", "ts", "tsx", "jsx", "json", "toml", "yaml", "yml", "xml", "csv",
    "log", "ini", "cfg", "sh", "bat", "ps1", "c", "h", "cpp", "hpp", "go", "java", "rb", "html",
    "css", // 图片
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico",
];

/// 该扩展名是否走内置预览。
pub fn is_previewable(path: &Path) -> bool {
    match path.extension().map(|e| e.to_string_lossy().to_lowercase()) {
        Some(ext) => PREVIEWABLE_EXTENSIONS.contains(&ext.as_str()),
        None => false,
    }
}

/// 默认外部程序。Windows 用 `%COMSPEC%`，类 Unix 用 `$SHELL`，都取不到时给平台兜底值。
pub fn default_external_program() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

/// 打开决策表。`force_external` = 用户显式要求「用默认程序打开」，
/// 用来覆盖「已知类型→预览」的默认（DESIGN §4.7.2 规则 2）。
pub fn route_open(path: &Path, force_external: bool) -> OpenRoute {
    if !force_external && is_previewable(path) {
        OpenRoute::Preview
    } else {
        OpenRoute::External(default_external_program())
    }
}

/// 按 VPath 的版本（MCP 客户端给的是 VPath）。
pub fn route_open_vpath(path: &VPath, force_external: bool) -> OpenRoute {
    route_open(path.as_path(), force_external)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn text_file_routes_to_preview() {
        let p = PathBuf::from("/x/notes.md");
        assert_eq!(route_open(&p, false), OpenRoute::Preview);
    }

    #[test]
    fn uppercase_extension_still_previews() {
        let p = PathBuf::from("/x/README.MD");
        assert_eq!(route_open(&p, false), OpenRoute::Preview);
    }

    /// 音视频必须走外置：内置播放器需要 FFmpeg，而 DESIGN §4.7.4 明确不内置。
    #[test]
    fn video_routes_to_external_not_preview() {
        let p = PathBuf::from("/x/clip.mp4");
        match route_open(&p, false) {
            OpenRoute::External(_) => {}
            OpenRoute::Preview => panic!("视频不应走内置预览（不内嵌 FFmpeg）"),
        }
    }

    #[test]
    fn unknown_extension_routes_to_external() {
        let p = PathBuf::from("/x/thing.xyzzy");
        assert!(matches!(route_open(&p, false), OpenRoute::External(_)));
    }

    #[test]
    fn no_extension_routes_to_external() {
        let p = PathBuf::from("/x/Makefile");
        assert!(matches!(route_open(&p, false), OpenRoute::External(_)));
    }

    /// 强制外置必须能覆盖「已知类型→预览」的默认，否则右键「用默认程序打开」失效。
    #[test]
    fn force_external_overrides_previewable() {
        let p = PathBuf::from("/x/notes.md");
        assert!(matches!(route_open(&p, true), OpenRoute::External(_)));
    }

    #[test]
    fn default_program_is_non_empty() {
        assert!(!default_external_program().is_empty());
    }
}
