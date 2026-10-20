//! 插件宿主：WASM 沙箱 + 预览渲染器契约。
//!
//! 设计（DESIGN §4.8）：内置软件不塞进 EXE，每个是一个 WASM 插件；
//! 预览面板是插件宿主，按扩展名路由到插件渲染器。
//!
//! ## ABI
//!
//! 插件是一个 **core wasm 模块**（非 component model），约定：
//!
//! - 自定义段 `fmnr.plugin` 内放 JSON 清单：`{name, version, extensions}`；
//! - 导出 `fmnr_abi_version() -> i32`，须等于 [`ABI_VERSION`]；
//! - 导出 `memory`；
//! - 导出 `fmnr_alloc(len: i32) -> i32`：插件分配输入缓冲；
//! - 导出 `fmnr_render(ptr: i32, len: i32) -> i64`：返回 `(out_len << 32) | out_ptr`。
//!
//! ## 沙箱
//!
//! 宿主用**空 Linker** 实例化。插件若 `import` 任何东西，实例化就会失败——
//! 这是「插件碰不到文件系统/网络」的**结构性**保证，不是靠约定或审计。
//! 另外给每次调用装燃料表，死循环会耗尽燃料而不是挂住 UI 线程。

pub mod host;
pub mod registry;

use serde::{Deserialize, Serialize};

/// ABI 版本。插件与宿主不一致时拒绝加载，而不是带着语义漂移跑起来。
pub const ABI_VERSION: i32 = 1;

/// 清单所在的自定义段名。
pub const MANIFEST_SECTION: &str = "fmnr.plugin";

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("wasm 编译失败：{0}")]
    Compile(String),
    #[error("插件缺少清单段 `{MANIFEST_SECTION}`")]
    MissingManifest,
    #[error("清单不是合法 JSON：{0}")]
    BadManifest(String),
    #[error("ABI 版本不匹配：插件报 {found}，宿主要求 {expected}")]
    AbiMismatch { found: i32, expected: i32 },
    #[error("插件导入了宿主未提供的符号 `{0}`——沙箱不提供任何导入")]
    UnsandboxedImport(String),
    #[error("插件缺少导出 `{0}`")]
    MissingExport(String),
    #[error("插件执行出错：{0}")]
    Trap(String),
    #[error("插件疑似死循环：燃料耗尽")]
    OutOfFuel,
    #[error("插件返回的结果指针越界：ptr={ptr} len={len} mem={mem}")]
    BadResultPtr { ptr: u32, len: u32, mem: usize },
}

/// 插件清单。来自 `fmnr.plugin` 自定义段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// 插件接管的扩展名（不带点、小写）。
    pub extensions: Vec<String>,
}

impl Manifest {
    /// 从自定义段字节解析。扩展名统一小写去点，免得 `MD` 与 `md` 两套。
    pub fn parse(bytes: &[u8]) -> Result<Self, PluginError> {
        let mut m: Manifest =
            serde_json::from_slice(bytes).map_err(|e| PluginError::BadManifest(e.to_string()))?;
        m.extensions = m
            .extensions
            .iter()
            .map(|e| normalize_ext(e))
            .filter(|e| !e.is_empty())
            .collect();
        Ok(m)
    }

    pub fn handles(&self, ext: &str) -> bool {
        let want = normalize_ext(ext);
        !want.is_empty() && self.extensions.contains(&want)
    }
}

/// 扩展名归一：去首尾空白、去前导点、转小写。
///
/// `".MD"`、`"MD"`、`" md "` 必须等价。清单是手写的，多一个空格或大小写不同
/// 就会让插件被判定为「不处理这个扩展名」——而用户看到的现象是
/// 「插件装上了但预览还是老样子」，这种静默失配排查起来很贵。
pub fn normalize_ext(ext: &str) -> String {
    ext.trim().trim_start_matches('.').to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_normalizes_extensions() {
        let m =
            Manifest::parse(br#"{"name":"md","version":"1.0","extensions":[".MD"," Markdown "]}"#)
                .unwrap();
        assert_eq!(m.name, "md");
        assert_eq!(m.extensions, vec!["md", "markdown"]);
    }

    #[test]
    fn handles_is_case_and_dot_insensitive() {
        let m = Manifest::parse(br#"{"name":"md","version":"1.0","extensions":["md"]}"#).unwrap();
        assert!(m.handles("md"));
        assert!(m.handles(".MD"));
        assert!(!m.handles("txt"));
        assert!(!m.handles(""));
    }

    /// 未知字段要报错而不是静默忽略：清单里打错一个键，
    /// 用户看到的是「插件装上了但不生效」，那种排查成本很高。
    #[test]
    fn unknown_manifest_field_is_rejected() {
        let e =
            Manifest::parse(br#"{"name":"a","version":"1","extensions":[],"unknown_field":[]}"#)
                .unwrap_err();
        assert!(
            matches!(e, PluginError::BadManifest(_)),
            "应报清单错误：{e}"
        );
    }

    #[test]
    fn missing_manifest_field_is_rejected() {
        let e = Manifest::parse(br#"{"name":"a","version":"1"}"#).unwrap_err();
        assert!(matches!(e, PluginError::BadManifest(_)));
    }
}
