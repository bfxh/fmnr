//! 开机自启动**管理器**：把这台机器上所有自启动项列出来，用户级的能开关。
//!
//! 和 `autostart` 的分工：那个模块只管 fmnr 自己那一个值；这个管全部项。
//! 用户 2026-10-17 的原话是"所有的开机自启动都是可以调整的，自己在顶部不变"——
//! 意思是这件事不该藏在设置面板里，也不该只能管自己。
//!
//! 三条硬约束：
//!
//! 1. **只写 HKCU**。机器级（HKLM）项与公共启动文件夹只列出来、标"要管理员"，
//!    不提供按钮——静默失败比不给按钮糟，而提权不是这个程序该顺手做的事。
//! 2. **关别人之前先存原值**。关掉一项 = 把它的命令行抄进
//!    `HKCU\Software\fmnr\startup-off\<名字>`，**然后**才删 `Run` 里那个值。
//!    顺序反了的话，中途崩溃就把用户的自启动项弄丢了且无从恢复。
//! 3. **不动 `Run` 键本身**，只动里面的具名值（同 `autostart` 的共享键纪律）。

use crate::Error;
use fmnr_core::Result;

/// 用户级自启动键（可写）。
pub const USER_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 机器级自启动键（只列，不写）。
pub const MACHINE_RUN_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
/// 机器级 32 位视图。
pub const MACHINE_RUN_WOW_KEY: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
/// 关掉的项目的镜像位置。**我们独占这棵树**，所以撤销时可以整棵删。
pub const MIRROR_KEY: &str = r"Software\fmnr\startup-off";

/// 一项是从哪儿来的。决定它能不能被改。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `HKCU\...\Run`
    UserRun,
    /// 用户「启动」文件夹里的快捷方式
    UserStartupFolder,
    /// `HKLM\...\Run`
    MachineRun,
    /// `HKLM\...\WOW6432Node\...\Run`
    MachineRunWow,
    /// 公共「启动」文件夹
    CommonStartupFolder,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::UserRun => "当前用户 · 注册表 Run",
            Origin::UserStartupFolder => "当前用户 · 启动文件夹",
            Origin::MachineRun => "本机所有用户 · 注册表 Run",
            Origin::MachineRunWow => "本机所有用户 · 注册表 Run(32)",
            Origin::CommonStartupFolder => "本机所有用户 · 公共启动文件夹",
        }
    }

    /// 只有当前用户作用域的东西不需要管理员就能改。
    pub fn editable(self) -> bool {
        matches!(self, Origin::UserRun | Origin::UserStartupFolder)
    }

    /// 不可编辑时给用户的说法。**要说清为什么**，只说"不能改"会让人以为程序坏了。
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Origin::UserRun | Origin::UserStartupFolder => None,
            Origin::MachineRun | Origin::MachineRunWow | Origin::CommonStartupFolder => {
                Some("机器级项要管理员权限才能改；fmnr 不顺手提权，请到该程序自己的设置里关")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupItem {
    pub name: String,
    pub command: String,
    pub origin: Origin,
    /// true = 这一项是被本程序关掉的，镜像里存着原值，可以开回来。
    pub turned_off_by_us: bool,
}

/// 一次改动要做的操作。**顺序即语义**，所以用列表而不是结构体字段表达。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// 把原值抄进我们的镜像键
    SaveMirror {
        name: String,
        command: String,
    },
    /// 删掉镜像里的那份
    DropMirror {
        name: String,
    },
    /// 删掉 `Run` 下的具名值（注意：是值，不是键）
    DeleteUserValue {
        subkey: String,
        name: String,
    },
    /// 把值写回 `Run`
    WriteUserValue {
        subkey: String,
        name: String,
        command: String,
    },
    /// 把启动文件夹里的快捷方式挪走/挪回（用改名实现，不删文件）
    HideStartupFile {
        path: String,
    },
    RestoreStartupFile {
        from: String,
        to: String,
    },
}

/// 关掉一项的操作序列。
///
/// 不可编辑的来源返回空——调用方据此不给按钮，而不是给一个会失败的按钮。
pub fn off_ops(item: &StartupItem) -> Vec<Op> {
    if !item.origin.editable() {
        return Vec::new();
    }
    match item.origin {
        Origin::UserRun => vec![
            // 先存后删：反过来会在"存之前崩掉"这种情形下永久丢掉用户的项
            Op::SaveMirror {
                name: item.name.clone(),
                command: item.command.clone(),
            },
            Op::DeleteUserValue {
                subkey: USER_RUN_KEY.to_string(),
                name: item.name.clone(),
            },
        ],
        Origin::UserStartupFolder => vec![Op::HideStartupFile {
            path: item.command.clone(),
        }],
        _ => Vec::new(),
    }
}

/// 开回一项的操作序列。
pub fn on_ops(item: &StartupItem) -> Vec<Op> {
    if !item.origin.editable() {
        return Vec::new();
    }
    match item.origin {
        Origin::UserRun => vec![
            Op::WriteUserValue {
                subkey: USER_RUN_KEY.to_string(),
                name: item.name.clone(),
                command: item.command.clone(),
            },
            Op::DropMirror {
                name: item.name.clone(),
            },
        ],
        Origin::UserStartupFolder => match item.command.strip_suffix(".fmnr-off") {
            Some(base) => vec![Op::RestoreStartupFile {
                from: item.command.clone(),
                to: base.to_string(),
            }],
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// 列出这台机器上能列到的自启动项。
///
/// 某一处读不到（没有权限、键不存在）不影响其余：它进 `warnings` 而不是让整份列表失败——
/// 但也不能装作它空。
#[derive(Debug, Clone, Default)]
pub struct StartupInventory {
    pub items: Vec<StartupItem>,
    pub warnings: Vec<String>,
}

#[cfg(not(windows))]
pub fn list() -> Result<StartupInventory> {
    Err(Error::Unsupported("自启动管理目前只实现了 Windows".into()))
}

#[cfg(windows)]
pub fn list() -> Result<StartupInventory> {
    let mut inv = StartupInventory::default();
    let mirrors = mirror_entries();

    for (origin, hive, subkey) in [
        (
            Origin::UserRun,
            windows::Win32::System::Registry::HKEY_CURRENT_USER,
            USER_RUN_KEY,
        ),
        (
            Origin::MachineRun,
            windows::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            MACHINE_RUN_KEY,
        ),
        (
            Origin::MachineRunWow,
            windows::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            MACHINE_RUN_WOW_KEY,
        ),
    ] {
        match read_run_values(hive, subkey) {
            Ok(vals) => {
                for (name, command) in vals {
                    inv.items.push(StartupItem {
                        name,
                        command,
                        origin,
                        turned_off_by_us: false,
                    });
                }
            }
            Err(w) => inv.warnings.push(format!("{}：{w}", origin.label())),
        }
    }

    // 被我们关掉的项：从镜像里读出来，标成"已关"，让用户能开回去
    for (name, command) in mirrors {
        inv.items.push(StartupItem {
            name,
            command,
            origin: Origin::UserRun,
            turned_off_by_us: true,
        });
    }

    for (origin, dir) in [
        (Origin::UserStartupFolder, user_startup_dir()),
        (Origin::CommonStartupFolder, common_startup_dir()),
    ] {
        let d = match dir {
            Some(d) => d,
            None => continue,
        };
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(e) => {
                inv.warnings
                    .push(format!("{}：读不了（{e}）", origin.label()));
                continue;
            }
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            // 启动文件夹里常年躺着一个 `desktop.ini`（资源管理器的视图设置），
            // 它不是自启动项。列出来会让人以为我们要去关系统文件。
            if name.eq_ignore_ascii_case("desktop.ini") {
                continue;
            }
            let off = name.ends_with(".fmnr-off");
            inv.items.push(StartupItem {
                name: if off {
                    name.trim_end_matches(".fmnr-off").to_string()
                } else {
                    name
                },
                command: p.display().to_string(),
                origin,
                turned_off_by_us: off,
            });
        }
    }

    inv.items.sort_by_key(|a| a.name.to_lowercase());
    Ok(inv)
}

/// 关掉一项。按 `off_ops` 给的顺序执行。
#[cfg(windows)]
pub fn turn_off(item: &StartupItem) -> Result<usize> {
    let ops = off_ops(item);
    if ops.is_empty() {
        return Err(Error::Unsupported(
            item.origin
                .reason()
                .unwrap_or("这一项不能由本程序调整")
                .to_string(),
        ));
    }
    let mut done = 0;
    for op in ops {
        match op {
            Op::SaveMirror { name, command } => {
                crate::assoc::set_hkcu_str(MIRROR_KEY, &name, &command)?;
                done += 1;
            }
            Op::DeleteUserValue { subkey, name } => {
                crate::assoc::delete_hkcu_value(&subkey, &name)?;
                done += 1;
            }
            Op::HideStartupFile { path } => {
                std::fs::rename(&path, format!("{path}.fmnr-off"))
                    .map_err(|e| Error::Other(format!("挪开 {path} 失败：{e}")))?;
                done += 1;
            }
            other => {
                return Err(Error::Other(format!(
                    "不该出现在关闭序列里的操作：{other:?}"
                )))
            }
        }
    }
    Ok(done)
}

/// 开回一项。
#[cfg(windows)]
pub fn turn_on(item: &StartupItem) -> Result<usize> {
    let ops = on_ops(item);
    if ops.is_empty() {
        return Err(Error::Unsupported(
            item.origin
                .reason()
                .unwrap_or("这一项没有可恢复的记录")
                .to_string(),
        ));
    }
    let mut done = 0;
    for op in ops {
        match op {
            Op::WriteUserValue {
                subkey,
                name,
                command,
            } => {
                crate::assoc::set_hkcu_str(&subkey, &name, &command)?;
                done += 1;
            }
            Op::DropMirror { name } => {
                crate::assoc::delete_hkcu_value(MIRROR_KEY, &name)?;
                done += 1;
            }
            Op::RestoreStartupFile { from, to } => {
                std::fs::rename(&from, &to)
                    .map_err(|e| Error::Other(format!("恢复 {from} 失败：{e}")))?;
                done += 1;
            }
            other => {
                return Err(Error::Other(format!(
                    "不该出现在开启序列里的操作：{other:?}"
                )))
            }
        }
    }
    Ok(done)
}
#[cfg(windows)]
fn read_run_values(
    hive: windows::Win32::System::Registry::HKEY,
    subkey: &str,
) -> std::result::Result<Vec<(String, String)>, String> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, KEY_READ, REG_EXPAND_SZ, REG_SZ,
    };

    let sub = crate::assoc::wide(subkey);
    let mut h = HKEY::default();
    let rc = unsafe { RegOpenKeyExW(hive, PCWSTR(sub.as_ptr()), None, KEY_READ, &mut h) };
    if rc != ERROR_SUCCESS {
        return Err(format!("打不开（{rc:?}）"));
    }
    let mut out = Vec::new();
    let mut name_buf = [0u16; 512];
    let mut data = vec![0u8; 8192];
    let mut idx: u32 = 0;
    loop {
        let mut nlen = name_buf.len() as u32;
        let mut dlen = data.len() as u32;
        let mut ty: u32 = 0;
        let rc = unsafe {
            RegEnumValueW(
                h,
                idx,
                Some(PWSTR(name_buf.as_mut_ptr())),
                &mut nlen,
                None,
                Some(&mut ty),
                Some(data.as_mut_ptr()),
                Some(&mut dlen),
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        idx += 1;
        if rc != ERROR_SUCCESS {
            continue;
        }
        if ty != REG_SZ.0 && ty != REG_EXPAND_SZ.0 {
            continue;
        }
        let name = String::from_utf16_lossy(&name_buf[..nlen as usize]);
        let units: Vec<u16> = data[..(dlen as usize).min(data.len())]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        let command = String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_string();
        if !name.is_empty() && !command.is_empty() {
            out.push((name, command));
        }
    }
    unsafe {
        let _ = RegCloseKey(h);
    }
    Ok(out)
}

/// 我们镜像里存着的"被本程序关掉的"项。
#[cfg(windows)]
fn mirror_entries() -> Vec<(String, String)> {
    read_run_values(
        windows::Win32::System::Registry::HKEY_CURRENT_USER,
        MIRROR_KEY,
    )
    .unwrap_or_default()
}

/// 用户「启动」文件夹。从注册表的 `User Shell Folders` 读，不硬编码——
/// 那个位置可以被组策略重定向。
#[cfg(windows)]
fn user_startup_dir() -> Option<std::path::PathBuf> {
    let raw = crate::assoc::get_hkcu_str(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders",
        "Startup",
    )
    .ok()
    .flatten()?;
    Some(std::path::PathBuf::from(expand_vars(&raw)))
}

/// 公共「启动」文件夹。
#[cfg(windows)]
fn common_startup_dir() -> Option<std::path::PathBuf> {
    let raw = crate::assoc::get_hklm_str(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders",
        "Common Startup",
    )
    .ok()
    .flatten()?;
    Some(std::path::PathBuf::from(expand_vars(&raw)))
}

#[cfg(any(windows, test))]
/// `%NAME%` 展开。`std::env::var` 只认 Unix 风格的 `$NAME`，而注册表里的
/// `REG_EXPAND_SZ` 用 `%NAME%`，所以自己展开。认不出来就原样留着——
/// 宁可显示一个看得出没展开的路径，也不要猜一个把用户带到别处去。
fn expand_vars(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' {
            if let Some(end) = chars[i + 1..].iter().position(|c| *c == '%') {
                let name: String = chars[i + 1..i + 1 + end].iter().collect();
                match std::env::var(&name) {
                    Ok(val) if !val.is_empty() => out.push_str(&val),
                    // 展不开：原样留着两个百分号，让 UI 上看得见"这是没展开的串"
                    _ => {
                        out.push('%');
                        out.push_str(&name);
                        out.push('%');
                    }
                }
                i += end + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(origin: Origin, name: &str, cmd: &str) -> StartupItem {
        StartupItem {
            name: name.to_string(),
            command: cmd.to_string(),
            origin,
            turned_off_by_us: false,
        }
    }

    /// 这条是**安全不变量**：关掉一项必须先存原值再删。
    /// 顺序反了的话，"存之前崩掉"就把用户的自启动项永久弄丢了。
    #[test]
    fn turning_off_saves_before_it_deletes() {
        let ops = off_ops(&item(Origin::UserRun, "Foo", r"C:\a\foo.exe"));
        assert_eq!(
            ops,
            vec![
                Op::SaveMirror {
                    name: "Foo".into(),
                    command: r"C:\a\foo.exe".into()
                },
                Op::DeleteUserValue {
                    subkey: USER_RUN_KEY.into(),
                    name: "Foo".into()
                },
            ],
            "顺序必须是先镜像后删除"
        );
    }

    /// 删的必须是 `Run` 里的**具名值**，键本身永远不动——
    /// `Run` 里装着这台机器所有程序的自启动项。
    #[test]
    fn only_ever_deletes_a_named_value_never_the_key() {
        let ops = off_ops(&item(Origin::UserRun, "Foo", "x"));
        match &ops[1] {
            Op::DeleteUserValue { subkey, name } => {
                assert_eq!(subkey, USER_RUN_KEY);
                assert_eq!(name, "Foo");
                assert!(!name.is_empty(), "空名字会退化成删默认值");
            }
            other => panic!("期望删值，拿到 {other:?}"),
        }
    }

    #[test]
    fn machine_scope_is_read_only_with_a_reason() {
        for o in [
            Origin::MachineRun,
            Origin::MachineRunWow,
            Origin::CommonStartupFolder,
        ] {
            assert!(!o.editable(), "{o:?} 不该可编辑");
            assert!(o.reason().is_some(), "{o:?} 要说清为什么不能改");
            assert!(off_ops(&item(o, "X", "y")).is_empty(), "{o:?} 不该给出操作");
            assert!(on_ops(&item(o, "X", "y")).is_empty());
        }
        for o in [Origin::UserRun, Origin::UserStartupFolder] {
            assert!(o.editable(), "{o:?} 该可编辑");
            assert!(o.reason().is_none());
        }
    }

    /// 启动文件夹的"关"是改名，不是删文件——文件本身是用户的。
    #[test]
    fn startup_folder_is_hidden_by_rename_not_deleted() {
        let ops = off_ops(&item(
            Origin::UserStartupFolder,
            "Tool.lnk",
            r"C:\Start Menu\Tool.lnk",
        ));
        assert_eq!(
            ops,
            vec![Op::HideStartupFile {
                path: r"C:\Start Menu\Tool.lnk".into()
            }]
        );
        assert!(
            !ops.iter().any(|o| matches!(o, Op::DeleteUserValue { .. })),
            "启动文件夹那条绝不能走删注册表值的路径"
        );
        let back = on_ops(&item(
            Origin::UserStartupFolder,
            "Tool.lnk",
            r"C:\Start Menu\Tool.lnk.fmnr-off",
        ));
        assert_eq!(
            back,
            vec![Op::RestoreStartupFile {
                from: r"C:\Start Menu\Tool.lnk.fmnr-off".into(),
                to: r"C:\Start Menu\Tool.lnk".into()
            }]
        );
        // 没被我们关过的（不带那个后缀）不该给出"开回来"的操作
        assert!(on_ops(&item(
            Origin::UserStartupFolder,
            "Tool.lnk",
            r"C:\Start Menu\Tool.lnk"
        ))
        .is_empty());
    }

    #[test]
    fn turning_on_writes_back_before_dropping_the_mirror() {
        let ops = on_ops(&item(Origin::UserRun, "Foo", "cmd"));
        assert!(matches!(ops[0], Op::WriteUserValue { .. }), "先写回");
        assert!(matches!(ops[1], Op::DropMirror { .. }), "后清镜像");
    }

    #[test]
    fn percent_expansion_leaves_unknowns_alone() {
        std::env::set_var("FMNR_TEST_DIR", r"C:\fmnr-test");
        assert_eq!(
            expand_vars(r"%FMNR_TEST_DIR%\Startup"),
            r"C:\fmnr-test\Startup"
        );
        assert_eq!(expand_vars(r"%NOPE_AT_ALL%\x"), r"%NOPE_AT_ALL%\x");
        assert_eq!(expand_vars("no vars"), "no vars");
        assert_eq!(expand_vars("trailing %UNCLOSED"), "trailing %UNCLOSED");
    }

    /// 真机：列一份自启动清单，确认读得到、且每一项都带来源。只读，不动任何键。
    #[cfg(windows)]
    #[test]
    fn listing_the_real_machine_works_and_is_wellformed() {
        let inv = list().expect("列自启动项不该失败");
        assert!(
            inv.items.iter().all(|i| !i.name.trim().is_empty()),
            "每条都该有名字"
        );
        assert!(
            inv.items.iter().all(|i| !i.command.trim().is_empty()),
            "每条都该有命令行或路径"
        );
        // 本机实测 HKCU Run 有 9 项；只断言"不止一条"以免换机就红
        assert!(
            inv.items.len() >= 2,
            "本机至少该列出几条，拿到 {}",
            inv.items.len()
        );
        println!(
            "自启动项 {} 条，读不到的来源 {} 处",
            inv.items.len(),
            inv.warnings.len()
        );
    }
}
