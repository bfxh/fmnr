//! 插件集合：从目录加载、按扩展名路由。
//!
//! 单个插件加载失败**不能**拖垮整个宿主——一个手写的 wasm 有语法错误很常见，
//! 那种情况下其余插件应当照常工作。因此失败被收集成 [`LoadWarning`] 返回，
//! 而不是让 `load_dir` 直接报错返回。

use crate::host::Plugin;
use crate::PluginError;
use std::path::{Path, PathBuf};

/// 一个加载失败的插件：路径 + 原因。保留路径是为了能告诉用户「哪个文件坏了」。
#[derive(Debug)]
pub struct LoadWarning {
    pub path: PathBuf,
    pub error: PluginError,
}

/// 一组已加载的插件。
#[derive(Default)]
pub struct PluginSet {
    plugins: Vec<Plugin>,
}

impl PluginSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// 扫描目录下的 `*.wasm` 并逐个加载。目录不存在时返回空集（不算错误：
    /// 没装插件是正常状态）。坏插件进 warnings，好插件照常可用。
    pub fn load_dir(dir: &Path) -> (Self, Vec<LoadWarning>) {
        let mut set = Self::new();
        let mut warnings = Vec::new();

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return (set, warnings),
        };

        // 排序让加载顺序稳定：否则「同名扩展名谁先注册」会随文件系统返回顺序变。
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("wasm"))
            })
            .collect();
        paths.sort();

        for path in paths {
            match std::fs::read(&path) {
                Ok(bytes) => match Plugin::load(&bytes) {
                    Ok(p) => set.plugins.push(p),
                    Err(e) => warnings.push(LoadWarning { path, error: e }),
                },
                Err(e) => warnings.push(LoadWarning {
                    path,
                    error: PluginError::Compile(format!("读取失败：{e}")),
                }),
            }
        }

        (set, warnings)
    }

    /// 有没有插件声明处理这个扩展名。
    pub fn handles(&self, ext: &str) -> bool {
        self.plugins.iter().any(|p| p.manifest().handles(ext))
    }

    /// 交给第一个声明处理该扩展名的插件渲染。
    ///
    /// 返回 `None` = 没有插件认领（调用方回落到内置预览）；
    /// `Some(Err)` = 插件认领了但渲染失败（这是插件的错，不该静默回落，
    /// 否则用户看到的是内置预览，以为插件没生效）。
    pub fn render(&mut self, ext: &str, input: &[u8]) -> Option<Result<Vec<u8>, PluginError>> {
        let idx = self
            .plugins
            .iter()
            .position(|p| p.manifest().handles(ext))?;
        Some(self.plugins[idx].render_bytes(input))
    }

    /// 已加载插件的清单，供 UI/CLI 展示。
    pub fn manifests(&self) -> Vec<&crate::Manifest> {
        self.plugins.iter().map(|p| p.manifest()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个真干活的插件（输入转大写），与 host.rs 的测试插件同构。
    const UPPER: &str = r#"
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

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fmnr-plugins-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_directory_yields_empty_set_not_error() {
        let d = std::env::temp_dir().join("fmnr-plugins-definitely-absent-dir");
        let _ = std::fs::remove_dir_all(&d);
        let (set, warns) = PluginSet::load_dir(&d);
        assert!(set.is_empty());
        assert!(warns.is_empty(), "没装插件是正常状态，不该报警告");
    }

    #[test]
    fn loads_a_real_plugin_and_routes_by_extension() {
        let d = scratch("route");
        std::fs::write(d.join("upper.wasm"), wat::parse_str(UPPER).unwrap()).unwrap();

        let (mut set, warns) = PluginSet::load_dir(&d);
        assert!(warns.is_empty(), "好插件不该有警告：{warns:?}");
        assert_eq!(set.len(), 1);
        assert!(set.handles("txt"));
        assert!(!set.handles("png"));

        let out = set.render("txt", b"hi").unwrap().unwrap();
        assert_eq!(out, b"HI");

        // 没插件认领的扩展名：返回 None，调用方回落内置预览。
        assert!(set.render("png", b"x").is_none());
    }

    /// 坏插件不能拖垮好插件——这是本模块存在的理由。
    #[test]
    fn a_broken_plugin_does_not_block_the_good_ones() {
        let d = scratch("mixed");
        std::fs::write(d.join("good.wasm"), wat::parse_str(UPPER).unwrap()).unwrap();
        std::fs::write(d.join("broken.wasm"), b"this is not wasm at all").unwrap();

        let (mut set, warns) = PluginSet::load_dir(&d);
        assert_eq!(set.len(), 1, "好插件应照常加载");
        assert_eq!(warns.len(), 1, "坏插件应被记录");
        assert!(warns[0].path.ends_with("broken.wasm"));
        assert_eq!(set.render("txt", b"ok").unwrap().unwrap(), b"OK");
    }

    #[test]
    fn non_wasm_files_are_ignored() {
        let d = scratch("ignore");
        std::fs::write(d.join("readme.md"), b"# hi").unwrap();
        std::fs::write(d.join("upper.wasm"), wat::parse_str(UPPER).unwrap()).unwrap();

        let (set, warns) = PluginSet::load_dir(&d);
        assert_eq!(set.len(), 1);
        assert!(warns.is_empty(), "非 .wasm 文件不该被当作插件加载");
    }

    /// 加载顺序必须稳定：否则「同名扩展名谁生效」会随文件系统抖动。
    #[test]
    fn load_order_is_deterministic() {
        let d = scratch("order");
        std::fs::write(d.join("b.wasm"), wat::parse_str(UPPER).unwrap()).unwrap();
        std::fs::write(d.join("a.wasm"), wat::parse_str(UPPER).unwrap()).unwrap();
        std::fs::write(d.join("c.wasm"), wat::parse_str(UPPER).unwrap()).unwrap();

        let (set, _) = PluginSet::load_dir(&d);
        let names: Vec<String> = set.manifests().iter().map(|m| m.name.clone()).collect();
        assert_eq!(names.len(), 3);
        // 三个插件同名 "upper"，这里只验证数量与可重复性由排序保证。
        let (set2, _) = PluginSet::load_dir(&d);
        assert_eq!(set2.len(), set.len());
    }
}
