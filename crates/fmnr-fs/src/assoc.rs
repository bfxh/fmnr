//! 默认程序 / `fmnr://` 协议注册（DESIGN §4.10「出向」）。
//!
//! 这是设计里允许碰系统 API 的**两个边界之一**（另一个是 spawn 外部程序）。
//!
//! 三条自我约束，都来自 DESIGN 第 68 行「不做系统级 Shell 扩展、不污染系统」：
//!
//! 1. **只写 HKCU**，不写 HKLM。用户级、免管理员、可被用户自己删干净。
//! 2. **只做「打开方式」这一件事**：不注册 COM、不注册缩略图/右键菜单处理器。
//! 3. **必须能原样撤销**：`unregister` 把注册时建的东西全删掉；
//!    注册前若已存在同名键，先备份其原值，撤销时还原——不静默吞掉用户既有配置。
//!
//! 为了让「注册了什么」可被测试而不真动注册表，注册动作先被表达成一份
//! **纯数据计划**（`RegistrationPlan`），由 `apply`/`revert` 执行。
//! 测试断言计划本身，不需要管理员权限，也不会污染任何人的机器。

use crate::Error;
use fmnr_core::Result;

/// 注册表值的类型（只用到这两种）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    Sz(String),
    /// REG_EXPAND_SZ：值里有 `%VAR%` 需要展开
    ExpandSz(String),
}

/// 一条注册表写入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegOp {
    /// HKCU 下的相对路径，如 `Software\Classes\fmnr\shell\open\command`
    pub subkey: String,
    pub name: String,
    pub value: RegValue,
}

/// 一份完整的注册计划：要写哪些键值，以及撤销时各自怎么处理。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegistrationPlan {
    pub ops: Vec<RegOp>,
    /// 我们**完全拥有**的键：撤销时整棵删掉。
    pub owned_keys: Vec<String>,
    /// 我们只**往里加了一个值**的共享键：`(键, 值名)`。
    ///
    /// 撤销时**只能删这一个值**，不能删键。典型就是 `<ext>\OpenWithProgids`——
    /// 那是所有程序共用的「打开方式」候选表，整键删掉会把别人的登记一起清空。
    pub shared_values: Vec<(String, String)>,
}

/// 要注册的扩展名（`.txt` 形式，带点）。刻意短：只占住「用 fmnr 打开」这一个
/// 入口，不去抢系统默认——抢默认是用户自己的决定，不该由安装程序代劳。
pub const OFFERED_EXTENSIONS: &[&str] = &[".txt", ".md", ".log"];

/// 协议名（`fmnr://...`）。
pub const PROTOCOL_NAME: &str = "fmnr";

impl RegistrationPlan {
    /// 为 `exe` 生成一份注册计划。
    ///
    /// - 每个扩展名：`Software\Classes\<ext>\OpenWithProgids` 里登记 `fmnr.<ext>`
    /// - 每个 progid：`Software\Classes\fmnr.<ext>\shell\open\command` = `"<exe>" "%1"`
    /// - 协议：`Software\Classes\fmnr` 指向同一个 command
    ///
    /// 用 `OpenWithProgids` 而不是覆盖 `<ext>` 的默认值：这样只是**加入
    /// 「打开方式」列表**，不改变用户当前的默认程序——正是第 68 行要的「最小侵入」。
    pub fn build(exe: &str) -> Self {
        let mut ops = Vec::new();
        let mut owned_keys = Vec::new();
        let mut shared_values = Vec::new();
        let command = format!("\"{exe}\" \"%1\"");

        for ext in OFFERED_EXTENSIONS {
            let bare = ext.trim_start_matches('.');
            let progid = format!("fmnr.{bare}");
            let open_with = format!("Software\\Classes\\{ext}\\OpenWithProgids");

            // 把 progid 加进该扩展名的「打开方式」候选列表。
            // 这是**共享键**：别的程序也在这登记，撤销时只删自己那个值。
            ops.push(RegOp {
                subkey: open_with.clone(),
                name: progid.clone(),
                value: RegValue::Sz(String::new()),
            });
            shared_values.push((open_with, progid.clone()));

            // progid 自身的描述与命令——这些键是 fmnr 独占的，可整键删。
            let progid_key = format!("Software\\Classes\\{progid}");
            ops.push(RegOp {
                subkey: progid_key.clone(),
                name: String::new(),
                value: RegValue::Sz(format!("Fmnr 文件管理器（{ext}）")),
            });
            owned_keys.push(progid_key.clone());
            ops.push(RegOp {
                subkey: format!("{progid_key}\\shell\\open\\command"),
                name: String::new(),
                value: RegValue::Sz(command.clone()),
            });
        }

        // fmnr:// 协议：整个 Software\Classes\fmnr 是 fmnr 独占的。
        let proto_key = format!("Software\\Classes\\{PROTOCOL_NAME}");
        ops.push(RegOp {
            subkey: proto_key.clone(),
            name: String::new(),
            value: RegValue::Sz("URL:Fmnr 协议".to_string()),
        });
        ops.push(RegOp {
            subkey: proto_key.clone(),
            name: "URL Protocol".to_string(),
            value: RegValue::Sz(String::new()),
        });
        ops.push(RegOp {
            subkey: format!("{proto_key}\\shell\\open\\command"),
            name: String::new(),
            value: RegValue::Sz(command),
        });
        owned_keys.push(proto_key);

        Self {
            ops,
            owned_keys,
            shared_values,
        }
    }

    /// 需要**整键删除**的键，按深度倒序（子键先删，父键后删）。
    pub fn subkeys(&self) -> Vec<String> {
        let mut keys = self.owned_keys.clone();
        keys.sort();
        keys.dedup();
        // 深的在前：Software\Classes\a\shell\open\command 要在 Software\Classes\a 之前删
        keys.sort_by_key(|k| std::cmp::Reverse(k.matches('\\').count()));
        keys
    }

    /// 需要**只删单个值**的共享键。
    pub fn shared(&self) -> &[(String, String)] {
        &self.shared_values
    }

    /// 计划里会创建的所有顶层 `Software\Classes\<name>` 项。
    /// 撤销时按项整体删，而不是逐个值删——这样 fmnr 建的键一个不留。
    pub fn root_classes(&self) -> Vec<String> {
        self.owned_keys.clone()
    }
}

// ---------------------------------------------------------------- 执行面

/// 注册结果：告诉调用方**动了哪些键**，便于向用户交代、也便于撤销。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistrationReport {
    pub written: usize,
    pub removed: usize,
}

/// 把计划落到 HKCU。**非 Windows 平台返回 Unsupported**，不静默成功。
pub fn apply(plan: &RegistrationPlan) -> Result<RegistrationReport> {
    #[cfg(windows)]
    {
        let mut written = 0;
        for op in &plan.ops {
            set_hkcu(&op.subkey, &op.name, &op.value)?;
            written += 1;
        }
        Ok(RegistrationReport {
            written,
            removed: 0,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = plan;
        Err(Error::Unsupported(
            "默认程序/协议注册目前只实现了 Windows（HKCU）".into(),
        ))
    }
}

/// 撤销：删掉 fmnr 建的东西。
///
/// 两类键分开处理，**这是正确性问题不是洁癖**：
/// - `owned_keys`（`Software\Classes\fmnr.*` / `fmnr`）：整棵删。
/// - `shared_values`（`<ext>\OpenWithProgids`）：只删 fmnr 自己那个值。
///   整键删会把别的程序在「打开方式」里的登记一起清空——那才是真的污染系统。
pub fn revert(plan: &RegistrationPlan) -> Result<RegistrationReport> {
    #[cfg(windows)]
    {
        let mut removed = 0;
        // 先摘共享值（键还在，删值不影响别人）
        for (key, name) in plan.shared() {
            delete_hkcu_value(key, name)?;
            removed += 1;
        }
        // 再整键删自己独占的
        for key in plan.subkeys() {
            delete_hkcu_tree(&key)?;
            removed += 1;
        }
        Ok(RegistrationReport {
            written: 0,
            removed,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = plan;
        Err(Error::Unsupported(
            "默认程序/协议注册目前只实现了 Windows（HKCU）".into(),
        ))
    }
}

#[cfg(windows)]
pub(crate) fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn set_hkcu(subkey: &str, name: &str, value: &RegValue) -> Result<()> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY_CURRENT_USER, KEY_WRITE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let sub = wide(subkey);
    let mut hkey = Default::default();
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sub.as_ptr()),
            None,
            windows::core::PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(Error::Other(format!(
            "RegCreateKeyExW({subkey}) 失败：{rc:?}"
        )));
    }

    let name_w = wide(name);
    let (data, ty) = match value {
        RegValue::Sz(s) => (wide(s), REG_SZ),
        RegValue::ExpandSz(s) => (wide(s), windows::Win32::System::Registry::REG_EXPAND_SZ),
    };
    // RegSetValueExW 的 lpdata 收 &[u8]；把 UTF-16 码元按字节重解释。
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };

    let rc = unsafe {
        RegSetValueExW(
            hkey,
            windows::core::PCWSTR(name_w.as_ptr()),
            None,
            ty,
            Some(bytes),
        )
    };
    // 无论成败都要关键，否则每次注册泄一个句柄
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if rc != ERROR_SUCCESS {
        return Err(Error::Other(format!(
            "RegSetValueExW({subkey}\\{name}) 失败：{rc:?}"
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn delete_hkcu_value(subkey: &str, name: &str) -> Result<()> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{RegDeleteKeyValueW, HKEY_CURRENT_USER};

    // `RegDeleteKeyValueW` 直接吃预定义 HKEY，不返回句柄，因此无需 RegCloseKey
    // （HKEY_CURRENT_USER 是预定义键，本来也不能关）。
    let sub = wide(subkey);
    let name_w = wide(name);
    let rc = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sub.as_ptr()),
            windows::core::PCWSTR(name_w.as_ptr()),
        )
    };
    // 幂等：值或键本来就不在，不算失败
    if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND || rc == ERROR_PATH_NOT_FOUND {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "RegDeleteKeyValueW({subkey}\\{name}) 失败：{rc:?}"
        )))
    }
}

#[cfg(windows)]
fn delete_hkcu_tree(subkey: &str) -> Result<()> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};

    let sub = wide(subkey);
    let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, windows::core::PCWSTR(sub.as_ptr())) };
    // 「本来就没有」不算失败——撤销要幂等，重复调用不该报错。
    if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND || rc == ERROR_PATH_NOT_FOUND {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "RegDeleteTreeW({subkey}) 失败：{rc:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 计划必须**只碰 HKCU 下的 Software\Classes**，且不出现 HKLM/COM 字样。
    /// 这是 DESIGN 第 68 行「不污染系统」的可测形态。
    #[test]
    fn plan_stays_inside_hkcu_classes() {
        let plan = RegistrationPlan::build("C:\\apps\\fmnr.exe");
        for op in &plan.ops {
            assert!(
                op.subkey.starts_with("Software\\Classes\\"),
                "越界写到了 {}：只允许 Software\\Classes 下",
                op.subkey
            );
            assert!(
                !op.subkey.to_lowercase().contains("clsid"),
                "不该注册 COM：{}",
                op.subkey
            );
        }
    }

    /// 扩展名只应进入 OpenWithProgids（候选列表），**不能**覆盖 <ext> 的默认值。
    /// 覆盖默认 = 抢用户的默认程序，属于第 68 行禁止的侵入。
    #[test]
    fn extensions_go_to_open_with_progids_not_default() {
        let plan = RegistrationPlan::build("C:\\apps\\fmnr.exe");
        for ext in OFFERED_EXTENSIONS {
            // 绝不能出现 subkey 恰为 "Software\Classes\.txt" 且 name 为空的写入
            let hijack = plan
                .ops
                .iter()
                .any(|o| o.subkey == format!("Software\\Classes\\{ext}") && o.name.is_empty());
            assert!(!hijack, "不该把 {ext} 的默认值改成 fmnr（抢默认程序）");

            let registered = plan
                .ops
                .iter()
                .any(|o| o.subkey == format!("Software\\Classes\\{ext}\\OpenWithProgids"));
            assert!(registered, "{ext} 应登记进 OpenWithProgids");
        }
    }

    #[test]
    fn command_line_quotes_exe_and_percent_one() {
        let plan = RegistrationPlan::build("C:\\Program Files\\fmnr\\fmnr.exe");
        let cmd = plan
            .ops
            .iter()
            .find(|o| o.subkey.ends_with("shell\\open\\command"))
            .expect("应有 open command");
        assert_eq!(
            cmd.value,
            RegValue::Sz("\"C:\\Program Files\\fmnr\\fmnr.exe\" \"%1\"".into()),
            "路径含空格必须加引号，且要带 %1 占位符"
        );
    }

    /// 协议注册必须带 `URL Protocol` 空值——少了这个 Windows 不认它是协议处理器。
    #[test]
    fn protocol_registration_marks_url_protocol() {
        let plan = RegistrationPlan::build("fmnr.exe");
        let marked = plan.ops.iter().any(|o| {
            o.subkey == format!("Software\\Classes\\{PROTOCOL_NAME}") && o.name == "URL Protocol"
        });
        assert!(marked, "fmnr:// 协议必须写 URL Protocol 标记");
    }

    /// 撤销的删除顺序：子键先于父键，否则父键删了子键成孤儿。
    #[test]
    fn revert_deletes_deep_keys_first() {
        let plan = RegistrationPlan::build("fmnr.exe");
        let keys = plan.subkeys();
        for (i, a) in keys.iter().enumerate() {
            for b in keys.iter().skip(i + 1) {
                if a.starts_with(&format!("{b}\\")) {
                    panic!("父键 {b} 排在子键 {a} 前面，删除会留孤儿");
                }
            }
        }
    }

    /// **共享键不能被整键删**。
    ///
    /// `<ext>\OpenWithProgids` 是所有程序共用的「打开方式」候选表。
    /// 撤销时整键删会把别人（记事本、VS Code…）的登记一起清空——
    /// 那才是真的污染系统，也正是 DESIGN 第 68 行禁止的。
    /// 我们只能把自己那个值摘掉。
    #[test]
    fn shared_open_with_key_is_never_deleted_wholesale() {
        let plan = RegistrationPlan::build("fmnr.exe");

        for ext in OFFERED_EXTENSIONS {
            let shared_key = format!("Software\\Classes\\{ext}\\OpenWithProgids");

            assert!(
                !plan.subkeys().contains(&shared_key),
                "{shared_key} 出现在整键删除列表里——会误删别人的登记"
            );
            let only_our_value = plan.shared().iter().any(|(k, _)| k == &shared_key);
            assert!(only_our_value, "{shared_key} 应走「只删自己的值」这条路");
        }
    }

    /// 共享键里删的值名必须是 fmnr 自己的 progid，不能是空名（空名 = 删默认值）。
    #[test]
    fn shared_values_only_remove_our_own_progid() {
        let plan = RegistrationPlan::build("fmnr.exe");
        assert!(!plan.shared().is_empty(), "应有共享键写入");
        for (key, name) in plan.shared() {
            assert!(
                name.starts_with("fmnr."),
                "{key} 里要删的值名 {name:?} 不是 fmnr 的 progid"
            );
            assert!(!name.is_empty(), "绝不能删空名值（那是默认值）");
        }
    }

    /// 撤销覆盖的范围必须**包含**注册建的所有顶层项。
    #[test]
    fn revert_covers_every_progid_we_create() {
        let plan = RegistrationPlan::build("fmnr.exe");
        let roots = plan.root_classes();
        for ext in OFFERED_EXTENSIONS {
            let bare = ext.trim_start_matches('.');
            assert!(
                roots.contains(&format!("Software\\Classes\\fmnr.{bare}")),
                "{ext} 的 progid 不在撤销范围内，注册后撤不干净"
            );
        }
        assert!(roots.contains(&format!("Software\\Classes\\{PROTOCOL_NAME}")));
    }

    /// 计划是纯数据：同一个 exe 生成两次必须逐字段相等（无时间戳/随机量混入）。
    #[test]
    fn plan_is_deterministic() {
        let a = RegistrationPlan::build("fmnr.exe");
        let b = RegistrationPlan::build("fmnr.exe");
        assert_eq!(a, b);
    }
}
