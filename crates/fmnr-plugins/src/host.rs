//! 沙箱实例化与渲染调用。
//!
//! 见 `lib.rs` 的 ABI 说明。这里的重点是**空 Linker**：插件若声明了任何
//! import，`instantiate` 就会失败——插件因此无法触及文件系统、网络或时钟。

use crate::{Manifest, PluginError, ABI_VERSION, MANIFEST_SECTION};
use wasmi::{Config, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

/// 每次渲染调用的燃料上限。
///
/// 解释执行下这是**唯一**能挡住死循环的手段：没有它，一个 `loop {}` 会把
/// UI 线程钉死。取值足够跑完正常的字符串处理，又不足以让死循环跑很久。
const FUEL_PER_CALL: u64 = 50_000_000;

/// 线性内存上限（16 MiB）。插件渲染的是预览文本，不需要更多；
/// 设上限是为了让「插件申请 4GB」这类输入在宿主侧就失败。
const MAX_MEMORY_BYTES: usize = 16 << 20;

/// 一个已加载、已实例化的插件。
///
/// 持有 `Store`/`Instance` 等 wasmi 句柄，因此不是 `Send`；UI 线程内使用。
pub struct Plugin {
    manifest: Manifest,
    store: Store<()>,
    instance: Instance,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    render: TypedFunc<(i32, i32), i64>,
}

/// 只打印清单。wasmi 句柄不实现 Debug，而且它们的内部状态对排查
/// 「插件为什么没生效」也没有帮助——能看的是「装上了哪个插件」。
impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl Plugin {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    /// 从 wasm 字节加载并实例化。任何一步失败都返回错误，不产生半个插件。
    pub fn load(wasm: &[u8]) -> Result<Self, PluginError> {
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm).map_err(|e| PluginError::Compile(e.to_string()))?;

        let manifest = read_manifest(&module)?;

        let mut store = Store::new(&engine, ());
        store
            .set_fuel(FUEL_PER_CALL)
            .map_err(|e| PluginError::Trap(e.to_string()))?;

        // 空 Linker：模块声明的每个 import 都会在实例化时找不到而失败。
        // 这是沙箱的结构性保证，而不是「约定插件不要做坏事」。
        let linker = Linker::<()>::new(&engine);
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| PluginError::Compile(e.to_string()))?
            .start(&mut store)
            .map_err(|e| PluginError::Compile(e.to_string()))?;

        verify_abi(&instance, &mut store)?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| PluginError::MissingExport("memory".into()))?;

        let mem_bytes = memory.data_size(&store);
        if mem_bytes > MAX_MEMORY_BYTES {
            return Err(PluginError::Compile(format!(
                "插件线性内存 {mem_bytes} 字节超过上限 {MAX_MEMORY_BYTES}"
            )));
        }

        let alloc = instance
            .get_func(&store, "fmnr_alloc")
            .ok_or_else(|| PluginError::MissingExport("fmnr_alloc".into()))?
            .typed::<i32, i32>(&store)
            .map_err(|e| PluginError::Trap(e.to_string()))?;

        let render = instance
            .get_func(&store, "fmnr_render")
            .ok_or_else(|| PluginError::MissingExport("fmnr_render".into()))?
            .typed::<(i32, i32), i64>(&store)
            .map_err(|e| PluginError::Trap(e.to_string()))?;

        Ok(Self {
            manifest,
            store,
            instance,
            memory,
            alloc,
            render,
        })
    }

    /// 把输入字节交给插件，取回它渲染出的字节。
    ///
    /// 每次调用都**重置燃料**，这样一个插件卡死只影响这一次调用，
    /// 不会因为上一次用光了燃料就让后续所有调用都失败。
    pub fn render_bytes(&mut self, input: &[u8]) -> Result<Vec<u8>, PluginError> {
        self.store
            .set_fuel(FUEL_PER_CALL)
            .map_err(|e| PluginError::Trap(e.to_string()))?;

        let len = i32::try_from(input.len())
            .map_err(|_| PluginError::Trap(format!("输入过长：{} 字节", input.len())))?;

        let ptr = self.alloc.call(&mut self.store, len).map_err(map_trap)?;
        if ptr < 0 {
            return Err(PluginError::Trap(format!("fmnr_alloc 返回负指针 {ptr}")));
        }
        let ptr = ptr as u32;

        let mem_size = self.memory.data_size(&self.store);
        let end = (ptr as usize)
            .checked_add(input.len())
            .ok_or(PluginError::BadResultPtr {
                ptr,
                len: input.len() as u32,
                mem: mem_size,
            })?;
        if end > mem_size {
            return Err(PluginError::BadResultPtr {
                ptr,
                len: input.len() as u32,
                mem: mem_size,
            });
        }

        self.memory
            .write(&mut self.store, ptr as usize, input)
            .map_err(|e| PluginError::Trap(e.to_string()))?;

        let packed = self
            .render
            .call(&mut self.store, (ptr as i32, len))
            .map_err(map_trap)?;

        // 返回约定：低 32 位是输出指针，高 32 位是输出长度。
        let out_ptr = (packed as u64 & 0xFFFF_FFFF) as u32;
        let out_len = ((packed as u64) >> 32) as u32;

        let mem_size = self.memory.data_size(&self.store);
        let out_end =
            (out_ptr as usize)
                .checked_add(out_len as usize)
                .ok_or(PluginError::BadResultPtr {
                    ptr: out_ptr,
                    len: out_len,
                    mem: mem_size,
                })?;
        if out_end > mem_size {
            return Err(PluginError::BadResultPtr {
                ptr: out_ptr,
                len: out_len,
                mem: mem_size,
            });
        }

        let mut out = vec![0u8; out_len as usize];
        self.memory
            .read(&self.store, out_ptr as usize, &mut out)
            .map_err(|e| PluginError::Trap(e.to_string()))?;
        Ok(out)
    }

    /// 供测试用：实例句柄（确认沙箱实例化确实产出了实例）。
    pub fn instance(&self) -> &Instance {
        &self.instance
    }
}

/// 从自定义段读清单。
fn read_manifest(module: &Module) -> Result<Manifest, PluginError> {
    for section in module.custom_sections() {
        if section.name() == MANIFEST_SECTION {
            return Manifest::parse(section.data());
        }
    }
    Err(PluginError::MissingManifest)
}

/// 校验 ABI 版本：导出必须存在，且**运行值**要等于宿主的 [`ABI_VERSION`]。
///
/// 只查导出存在是不够的——插件可以导出个返回 99 的函数。版本不符时必须拒绝，
/// 否则宿主会带着语义漂移去解释插件的返回体。
fn verify_abi(instance: &Instance, store: &mut Store<()>) -> Result<(), PluginError> {
    let func = instance
        .get_func(&*store, "fmnr_abi_version")
        .ok_or_else(|| PluginError::MissingExport("fmnr_abi_version".into()))?;
    let typed = func
        .typed::<(), i32>(&*store)
        .map_err(|e| PluginError::Trap(e.to_string()))?;
    let found = typed.call(&mut *store, ()).map_err(map_trap)?;
    if found != ABI_VERSION {
        return Err(PluginError::AbiMismatch {
            found,
            expected: ABI_VERSION,
        });
    }
    Ok(())
}

/// 把 wasmi 的 trap 归类：燃料耗尽要单独报，否则用户只看到「执行出错」。
fn map_trap(e: wasmi::Error) -> PluginError {
    let msg = e.to_string();
    if msg.contains("fuel") || msg.contains("Fuel") {
        PluginError::OutOfFuel
    } else {
        PluginError::Trap(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginError;

    /// 一个真干活的插件：把输入里的 ASCII 小写转大写。
    ///
    /// 用真变换而不是「原样返回」，是为了让测试能区分「插件真的读了输入」
    /// 与「宿主把输入自己吐回来了」——后者在恒等插件下无法察觉。
    const UPPERCASE_PLUGIN: &str = r#"
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
    (local $out i32)
    (local $i i32)
    (local $b i32)
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

    fn load(wat: &str) -> Result<Plugin, PluginError> {
        Plugin::load(&wat::parse_str(wat).expect("WAT 应能编译"))
    }

    #[test]
    fn loads_a_real_plugin_and_renders() {
        let mut p = load(UPPERCASE_PLUGIN).expect("应能加载");
        assert_eq!(p.manifest().name, "upper");
        assert!(p.manifest().handles("txt"));
        assert!(!p.manifest().handles("png"));

        let out = p.render_bytes(b"hello world").expect("应能渲染");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "HELLO WORLD",
            "插件应真的读到了输入并做了变换"
        );
    }

    /// 同一实例可以被反复调用（每次重置燃料）。
    #[test]
    fn render_can_be_called_repeatedly() {
        let mut p = load(UPPERCASE_PLUGIN).unwrap();
        assert_eq!(p.render_bytes(b"aa").unwrap(), b"AA");
        assert_eq!(p.render_bytes(b"bb").unwrap(), b"BB");
        assert_eq!(p.render_bytes(b"").unwrap(), b"");
    }

    /// 沙箱的结构性保证：插件只要声明了 import，实例化就失败。
    /// 这条不成立的话，「插件碰不到文件系统」就只是句口头承诺。
    #[test]
    fn plugin_with_imports_is_refused() {
        let wat = r#"
(module
  (@custom "fmnr.plugin" "{\"name\":\"evil\",\"version\":\"1.0\",\"extensions\":[\"txt\"]}")
  (import "env" "read_file" (func $read_file (param i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "fmnr_abi_version") (result i32) i32.const 1)
  (func (export "fmnr_alloc") (param i32) (result i32) i32.const 0)
  (func (export "fmnr_render") (param i32) (param i32) (result i64) i64.const 0))
"#;
        let e = load(wat).unwrap_err();
        assert!(
            matches!(e, PluginError::Compile(_)),
            "带 import 的插件必须加载失败，实际：{e}"
        );
    }

    #[test]
    fn abi_version_mismatch_is_refused() {
        let wat = r#"
(module
  (@custom "fmnr.plugin" "{\"name\":\"old\",\"version\":\"1.0\",\"extensions\":[\"txt\"]}")
  (memory (export "memory") 1)
  (func (export "fmnr_abi_version") (result i32) i32.const 99)
  (func (export "fmnr_alloc") (param i32) (result i32) i32.const 0)
  (func (export "fmnr_render") (param i32) (param i32) (result i64) i64.const 0))
"#;
        match load(wat) {
            Err(PluginError::AbiMismatch { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, ABI_VERSION);
            }
            other => panic!("应报 ABI 版本不符：{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn missing_manifest_is_refused() {
        let wat = r#"
(module
  (memory (export "memory") 1)
  (func (export "fmnr_abi_version") (result i32) i32.const 1)
  (func (export "fmnr_alloc") (param i32) (result i32) i32.const 0)
  (func (export "fmnr_render") (param i32) (param i32) (result i64) i64.const 0))
"#;
        assert!(matches!(
            load(wat).unwrap_err(),
            PluginError::MissingManifest
        ));
    }

    #[test]
    fn missing_required_export_is_refused() {
        // 有清单、有版本，但没有 memory。
        let wat = r#"
(module
  (@custom "fmnr.plugin" "{\"name\":\"nomem\",\"version\":\"1.0\",\"extensions\":[\"txt\"]}")
  (func (export "fmnr_abi_version") (result i32) i32.const 1)
  (func (export "fmnr_alloc") (param i32) (result i32) i32.const 0)
  (func (export "fmnr_render") (param i32) (param i32) (result i64) i64.const 0))
"#;
        match load(wat) {
            Err(PluginError::MissingExport(name)) => assert_eq!(name, "memory"),
            other => panic!("应报缺少导出：{:?}", other.map(|_| ())),
        }
    }

    /// 死循环必须被燃料挡住，而不是把宿主线程挂住。
    #[test]
    fn infinite_loop_runs_out_of_fuel() {
        let wat = r#"
(module
  (@custom "fmnr.plugin" "{\"name\":\"spin\",\"version\":\"1.0\",\"extensions\":[\"txt\"]}")
  (memory (export "memory") 1)
  (func (export "fmnr_abi_version") (result i32) i32.const 1)
  (func (export "fmnr_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "fmnr_render") (param i32) (param i32) (result i64)
    (loop $forever (br $forever))
    i64.const 0))
"#;
        let mut p = load(wat).expect("加载不该跑渲染，死循环不该在这里触发");
        match p.render_bytes(b"x") {
            Err(PluginError::OutOfFuel) => {}
            other => panic!("死循环应报燃料耗尽：{other:?}"),
        }
    }

    /// 插件返回越界指针时必须被宿主挡住，而不是读到别的内存。
    #[test]
    fn out_of_bounds_result_pointer_is_refused() {
        let wat = r#"
(module
  (@custom "fmnr.plugin" "{\"name\":\"oob\",\"version\":\"1.0\",\"extensions\":[\"txt\"]}")
  (memory (export "memory") 1)
  (func (export "fmnr_abi_version") (result i32) i32.const 1)
  (func (export "fmnr_alloc") (param i32) (result i32) i32.const 1024)
  (func (export "fmnr_render") (param i32) (param i32) (result i64)
    ;; 指针 0xFFFFFF00、长度 4096 —— 远超 64KiB 线性内存
    (i64.or (i64.shl (i64.const 4096) (i64.const 32)) (i64.const 0xFFFFFF00))))
"#;
        let mut p = load(wat).unwrap();
        match p.render_bytes(b"x") {
            Err(PluginError::BadResultPtr { .. }) => {}
            other => panic!("越界指针必须被拒：{other:?}"),
        }
    }

    /// 清单里声明了扩展名，宿主据此路由——契约的核心。
    #[test]
    fn manifest_drives_extension_routing() {
        let p = load(UPPERCASE_PLUGIN).unwrap();
        assert!(p.manifest().handles(".TXT"), "路由判定要大小写不敏感");
        assert!(!p.manifest().handles("md"));
    }
}
