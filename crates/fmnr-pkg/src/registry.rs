//! 已装程序底账：读注册表那三棵 `Uninstall` 树。
//!
//! 为什么要有这一层，而不是直接信包管理器：`winget upgrade` 只看得见 winget
//! 自己源里的东西，而本机实测 273 条已装记录里只有 2 条能从路径上看出是 winget 装的。
//! 也就是说"这台机器上装了什么"这个问题，注册表才是底账，包管理器只是一个来源。
//!
//! 分两层：`classify_family` / `dedup` 是纯函数（跨平台可测），
//! 读注册表那层只在 Windows 上存在。这样"是谁的包管理器"这条判据能被钉住，
//! 而不必在 CI 里真去动注册表。

use crate::github::repo_from_url;

/// 注册表里那条记录出自哪棵树。作用域不同，撤销方式就不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hive {
    /// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`
    Machine,
    /// 32 位视图（WOW6432Node）
    MachineWow,
    /// `HKCU\...`：用户级安装，免管理员
    User,
}

impl Hive {
    pub fn label(self) -> &'static str {
        match self {
            Hive::Machine => "机器级",
            Hive::MachineWow => "机器级(32)",
            Hive::User => "用户级",
        }
    }
}

/// 从卸载串/安装路径的形状推"是谁装它"。
///
/// 这是**启发式**，不是权威判定——注册表里没有"我用 winget 装的"这个字段
/// （实测 273 条里只有 2 条路径含 winget）。所以它只能回答"这看起来像什么"，
/// 不能拿来当卸载依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `msiexec` 驱动：Windows Installer 包
    Msi,
    /// 自带 `uninstall.exe`：Inno Setup / NSIS 那一类
    UninstallerExe,
    /// 装在 `WindowsApps` 下：Microsoft Store
    Store,
    /// 路径里有 scoop / chocolatey / winget 目录：第三方包管理器
    Scoop,
    Chocolatey,
    Winget,
    /// 上面都不像
    Unknown,
}

impl Family {
    pub fn label(self) -> &'static str {
        match self {
            Family::Msi => "Windows Installer (MSI)",
            Family::UninstallerExe => "自带卸载器 (Inno/NSIS)",
            Family::Store => "Microsoft Store",
            Family::Scoop => "scoop",
            Family::Chocolatey => "chocolatey",
            Family::Winget => "winget",
            Family::Unknown => "未知",
        }
    }
}

/// 一条已装记录。字段全用 `String`/`Option<String>`：
/// 它们要么直接来自注册表，要么是注册表串的派生值，中间不做任何"猜测式规范化"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    pub name: String,
    pub publisher: Option<String>,
    pub version: Option<String>,
    pub install_location: Option<String>,
    pub uninstall_string: Option<String>,
    pub quiet_uninstall_string: Option<String>,
    pub url: Option<String>,
    pub hive: Hive,
    pub family: Family,
    /// 注册表 URL 里认出来的 GitHub `owner/repo`（认不出为 None）。
    /// 上游版本查询只能从这一条入口进去——实测 273 条里只有 6 条有。
    pub github: Option<String>,
    /// 注册表里那一层键名（详情里交代"这条是从哪读来的"）。
    pub reg_key: Option<String>,
}

/// 纯函数：按形状分家族。顺序有意义——包管理器的痕迹比"有卸载器"更具体，
/// 所以先判路径特征再判 `msiexec`。
pub fn classify_family(install_location: Option<&str>, uninstall: Option<&str>) -> Family {
    let hay = format!(
        "{} {}",
        install_location.unwrap_or(""),
        uninstall.unwrap_or("")
    )
    .to_lowercase();
    if hay.contains("scoop") {
        return Family::Scoop;
    }
    if hay.contains("chocolatey") || hay.contains(r"\choco") {
        return Family::Chocolatey;
    }
    if hay.contains("winget") {
        return Family::Winget;
    }
    if hay.contains("windowsapps") {
        return Family::Store;
    }
    if hay.contains("msiexec") {
        return Family::Msi;
    }
    // Inno Setup 的卸载器叫 `unins000.exe`，**不含 "uninstall" 这个子串**；
    // NSIS 才叫 `Uninstall.exe`。只认后者的话，本机那 28 条自带卸载器的
    // 记录会全被判成 Unknown。
    if hay.contains(".exe") && (hay.contains("unins") || hay.contains("uninstall")) {
        return Family::UninstallerExe;
    }
    Family::Unknown
}

/// 同名条目可能同时出现在机器级和用户级（本机实测有）。
/// 这里**不静默合并**：两条都留着，由调用方按 hive 交代来源。
/// 合并会丢掉"同一个软件装了两次/两个作用域"这个事实，而那正是用户要看的。
pub fn dedup_by_key(apps: Vec<InstalledApp>) -> Vec<InstalledApp> {
    let mut out: Vec<InstalledApp> = Vec::with_capacity(apps.len());
    let mut seen = std::collections::HashSet::new();
    for a in apps {
        let id = (
            a.name.clone(),
            a.version.clone(),
            a.hive as u8,
            a.reg_key.clone(),
        );
        if seen.insert(id) {
            out.push(a);
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// 从一条注册表键的值集合造出 `InstalledApp`。纯函数，跨平台可测。
pub fn app_from_values(
    hive: Hive,
    reg_key: Option<String>,
    values: &std::collections::BTreeMap<String, String>,
) -> Option<InstalledApp> {
    let get = |k: &str| {
        values
            .get(k)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    // 没有 DisplayName 的多是系统组件补丁（`Update for ...`）和卸载残留，
    // 列出来只会淹没真正装过的软件。
    let name = get("DisplayName")?;
    let url = get("URLInfoAbout").or_else(|| get("HelpLink"));
    let family = classify_family(
        get("InstallLocation").as_deref(),
        get("UninstallString").as_deref(),
    );
    Some(InstalledApp {
        name,
        publisher: get("Publisher"),
        version: get("DisplayVersion"),
        install_location: get("InstallLocation"),
        uninstall_string: get("UninstallString"),
        quiet_uninstall_string: get("QuietUninstallString"),
        url: url.clone(),
        hive,
        family,
        github: url.as_deref().and_then(repo_from_url),
        reg_key,
    })
}

/// 把"某个 exe 路径"对回底账里的那条记录。
///
/// 取**最长**的 `InstallLocation` 前缀，而不是第一条命中的：
/// `D:\Apps\Foo` 和 `D:\Apps\Foo\plugins\Bar` 都可能装过东西，
/// 短的那个会把长的抢走，于是详情里写着错家的发布者。
/// 没有 `InstallLocation` 的（实测 273 条里 199 条没有）退一步看卸载串所在目录。
pub fn match_target<'a>(apps: &'a [InstalledApp], target: &str) -> Option<&'a InstalledApp> {
    let t = norm(target);
    if t.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &InstalledApp)> = None;
    for a in apps {
        let mut cand: Option<String> = a.install_location.as_deref().map(norm);
        if cand.is_none() {
            // 退到卸载串所在目录：Inno/NSIS 的卸载器就躺在程序目录里。
            cand = a
                .uninstall_string
                .as_deref()
                .and_then(|s| std::path::Path::new(s.trim_matches('"')).parent())
                .map(|p| norm(&p.display().to_string()));
        }
        let Some(c) = cand.filter(|c| !c.is_empty()) else {
            continue;
        };
        if t == c || t.starts_with(&c) {
            // 前缀要落在路径分隔上，否则 `D:\Apps\Foo` 会匹配到 `D:\Apps\FooBar\a.exe`
            let boundary_ok =
                t.len() == c.len() || matches!(t.as_bytes().get(c.len()), Some(b'\\') | Some(b'/'));
            if boundary_ok && best.map(|(n, _)| c.len() > n).unwrap_or(true) {
                best = Some((c.len(), a));
            }
        }
    }
    best.map(|(_, a)| a)
}

fn norm(s: &str) -> String {
    s.trim()
        .trim_matches('"')
        .replace('/', "\\")
        .to_ascii_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

/// 读三棵 `Uninstall` 树。非 Windows 返回 `Unsupported`（不静默返回空表——
/// 空表和"这台机器读不到"是两件不同的事）。
#[cfg(not(windows))]
pub fn list_installed() -> std::result::Result<Vec<InstalledApp>, crate::Error> {
    Err(crate::Error::Io(
        "已装程序底账目前只实现了 Windows（注册表 Uninstall 树）".into(),
    ))
}

#[cfg(windows)]
pub fn list_installed() -> std::result::Result<Vec<InstalledApp>, crate::Error> {
    let mut out = Vec::new();
    let trees = [
        (
            Hive::Machine,
            windows::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            Hive::MachineWow,
            windows::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            Hive::User,
            windows::Win32::System::Registry::HKEY_CURRENT_USER,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
    ];
    for (hive, root, subkey) in trees {
        // 某一棵树读不到（比如这台机器没有 32 位视图）不该让整份底账失败，
        // 但也不能装作它空：记进 tracing，UI 上按"读到的条数"说话。
        if let Err(e) = read_tree(hive, root, subkey, &mut out) {
            tracing::warn!("读注册表 {subkey} 失败：{e}");
        }
    }
    Ok(dedup_by_key(out))
}

#[cfg(windows)]
fn read_tree(
    hive: Hive,
    root: windows::Win32::System::Registry::HKEY,
    subkey: &str,
    out: &mut Vec<InstalledApp>,
) -> std::result::Result<(), crate::Error> {
    use std::collections::BTreeMap;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, KEY_READ,
    };

    let sub = wide(subkey);
    let mut hroot = HKEY::default();
    let rc = unsafe { RegOpenKeyExW(root, PCWSTR(sub.as_ptr()), None, KEY_READ, &mut hroot) };
    if rc != ERROR_SUCCESS {
        return Err(crate::Error::Io(format!(
            "RegOpenKeyExW({subkey}) 失败：{rc:?}"
        )));
    }

    let mut name_buf = [0u16; 512];
    let mut idx: u32 = 0;
    loop {
        let mut len = name_buf.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(
                hroot,
                idx,
                Some(PWSTR(name_buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        idx += 1;
        if rc != ERROR_SUCCESS {
            continue;
        }
        let key_name = String::from_utf16_lossy(&name_buf[..len as usize]);
        let mut hsub = HKEY::default();
        let kn = wide(&key_name);
        let rc = unsafe { RegOpenKeyExW(hroot, PCWSTR(kn.as_ptr()), None, KEY_READ, &mut hsub) };
        if rc != ERROR_SUCCESS {
            continue;
        }
        let mut values = BTreeMap::new();
        for (vname, vdata) in enumerate_values(hsub) {
            values.insert(vname, vdata);
        }
        unsafe {
            let _ = RegCloseKey(hsub);
        }
        if let Some(app) = app_from_values(hive, Some(key_name), &values) {
            out.push(app);
        }
    }
    unsafe {
        let _ = RegCloseKey(hroot);
    }
    Ok(())
}

/// 列一个键下的所有 REG_SZ / REG_EXPAND_SZ 值。
#[cfg(windows)]
fn enumerate_values(key: windows::Win32::System::Registry::HKEY) -> Vec<(String, String)> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{RegEnumValueW, REG_EXPAND_SZ, REG_SZ};

    let mut out = Vec::new();
    let mut name_buf = [0u16; 512];
    let mut data_buf: Vec<u8> = vec![0; 8192];
    let mut idx: u32 = 0;
    loop {
        let mut name_len = name_buf.len() as u32;
        let mut data_len = data_buf.len() as u32;
        // RegEnumValueW 的 lptype 收的是裸 u32（不是 REG_VALUE_TYPE），
        // 所以这里比的是两个常量的 .0。
        let mut ty: u32 = 0;
        let rc = unsafe {
            RegEnumValueW(
                key,
                idx,
                Some(PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut ty),
                Some(data_buf.as_mut_ptr()),
                Some(&mut data_len),
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
        let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        let units: Vec<u16> = data_buf[..(data_len as usize).min(data_buf.len())]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        let s = String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_string();
        if !s.is_empty() {
            out.push((name, s));
        }
    }
    out
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn family_from_shapes() {
        assert_eq!(
            classify_family(None, Some(r#"msiexec.exe /X {ABC}"#)),
            Family::Msi
        );
        assert_eq!(
            classify_family(
                Some(r"C:\Program Files\Foo"),
                Some(r"C:\Program Files\Foo\unins000.exe")
            ),
            Family::UninstallerExe
        );
        assert_eq!(
            classify_family(Some(r"C:\Users\x\scoop\apps\foo"), None),
            Family::Scoop
        );
        assert_eq!(
            classify_family(Some(r"C:\Program Files\Choco\foo"), None),
            Family::Chocolatey
        );
        assert_eq!(
            classify_family(
                Some(r"C:\Users\x\AppData\Local\Microsoft\WinGet\Packages\foo"),
                None
            ),
            Family::Winget
        );
        assert_eq!(
            classify_family(Some(r"C:\Program Files\WindowsApps\Bar_1.0"), None),
            Family::Store
        );
        assert_eq!(classify_family(None, None), Family::Unknown);
    }

    /// `msiexec` 那条包管理器的安装也会留下 msiexec 卸载串，
    /// 所以顺序是"先看包管理器痕迹，再看 msiexec"。这条锁住顺序。
    #[test]
    fn package_manager_hint_wins_over_msiexec() {
        assert_eq!(
            classify_family(
                Some(r"C:\Users\x\AppData\Local\Microsoft\WinGet\Packages\foo"),
                Some("msiexec.exe /X {ABC}")
            ),
            Family::Winget
        );
    }

    #[test]
    fn entry_without_display_name_is_dropped() {
        // 系统补丁项只有 PackageName/无 DisplayName，列出来只会淹掉真货
        assert!(
            app_from_values(Hive::Machine, None, &vals(&[("Publisher", "Microsoft")])).is_none()
        );
    }

    #[test]
    fn reads_the_fields_that_answer_whose_package_manager_it_is() {
        let a = app_from_values(
            Hive::User,
            Some(r"vendor.foo".into()),
            &vals(&[
                ("DisplayName", "Foo App"),
                ("DisplayVersion", "1.2.3"),
                ("Publisher", "Vendor"),
                ("InstallLocation", r"C:\Users\x\scoop\apps\foo\current"),
                (
                    "UninstallString",
                    r"C:\Users\x\scoop\apps\foo\current\uninstall.exe",
                ),
                ("URLInfoAbout", "https://github.com/vendor/foo"),
            ]),
        )
        .expect("有 DisplayName 就该成一条");
        assert_eq!(a.name, "Foo App");
        assert_eq!(a.version.as_deref(), Some("1.2.3"));
        assert_eq!(a.family, Family::Scoop);
        assert_eq!(a.github.as_deref(), Some("vendor/foo"));
        assert_eq!(a.hive, Hive::User);
        assert_eq!(a.reg_key.as_deref(), Some("vendor.foo"));
    }

    /// 同名同版本但作用域不同 ⇒ 两条都留。合并会丢掉"装了两次"这个事实。
    #[test]
    fn same_name_different_scope_is_not_merged() {
        let mk = |hive: Hive| {
            let mut a = app_from_values(
                hive,
                Some("k".into()),
                &vals(&[("DisplayName", "Dup"), ("DisplayVersion", "1.0")]),
            )
            .unwrap();
            a.reg_key = Some(format!("{hive:?}"));
            a
        };
        let out = dedup_by_key(vec![mk(Hive::Machine), mk(Hive::User), mk(Hive::Machine)]);
        assert_eq!(out.len(), 2, "机器级+用户级各一条，重复的第三条被去掉");
    }

    #[test]
    fn hive_labels_are_distinct() {
        let labels = [
            Hive::Machine.label(),
            Hive::MachineWow.label(),
            Hive::User.label(),
        ];
        assert_eq!(
            labels
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );
    }

    fn app(name: &str, loc: Option<&str>, uninstall: Option<&str>) -> InstalledApp {
        let mut v = vals(&[("DisplayName", name)]);
        for (k, x) in [("InstallLocation", loc), ("UninstallString", uninstall)] {
            if let Some(x) = x {
                v.insert(k.to_string(), x.to_string());
            }
        }
        app_from_values(Hive::Machine, Some(name.to_string()), &v).unwrap()
    }

    /// 最长前缀赢：短的那个会抢走长的那个的 exe，详情里就写错发布者/版本。
    #[test]
    fn longest_install_location_wins() {
        let outer = app("Outer", Some(r"D:\Apps\Foo"), None);
        let inner = app("Inner", Some(r"D:\Apps\Foo\plugins\Bar"), None);
        let apps = vec![outer.clone(), inner.clone()];
        let m = match_target(&apps, r"D:\Apps\Foo\plugins\Bar\bar.exe").unwrap();
        assert_eq!(m.name, "Inner");
        // 外层自己的 exe 还是要归外层
        assert_eq!(
            match_target(&apps, r"D:\Apps\Foo\foo.exe").unwrap().name,
            "Outer"
        );
    }

    /// `D:\Apps\Foo` 不能匹配 `D:\Apps\FooBar\a.exe`——前缀必须落在分隔符上。
    #[test]
    fn prefix_must_land_on_a_separator() {
        let apps = vec![app("Foo", Some(r"D:\Apps\Foo"), None)];
        assert!(match_target(&apps, r"D:\Apps\FooBar\a.exe").is_none());
        assert!(
            match_target(&apps, r"d:\apps\foo\a.exe").is_some(),
            "大小写不敏感"
        );
        assert!(
            match_target(&apps, r"D:/Apps/Foo/a.exe").is_some(),
            "正斜杠同样认"
        );
    }

    /// 199/273 条没有 InstallLocation；这类退到卸载串所在目录。
    #[test]
    fn falls_back_to_the_uninstaller_directory() {
        let apps = vec![app(
            "NoLoc",
            None,
            Some(r#""D:\Programs\NoLoc\unins000.exe""#),
        )];
        assert_eq!(
            match_target(&apps, r"D:\Programs\NoLoc\app.exe")
                .unwrap()
                .name,
            "NoLoc"
        );
    }

    #[test]
    fn empty_or_unrelated_targets_match_nothing() {
        let apps = vec![app("Foo", Some(r"D:\Apps\Foo"), None)];
        assert!(match_target(&apps, "").is_none());
        assert!(match_target(&apps, r"C:\Windows\notepad.exe").is_none());
        assert!(match_target(&[], r"D:\Apps\Foo\a.exe").is_none());
    }

    /// 真件闭环：开发机上那个快捷方式 → 目标 exe → 底账里应有 UniGetUI。
    /// 这条把 lnk 解析和底账匹配钉在一起，而不各自测自己的假设。
    #[cfg(windows)]
    #[test]
    fn real_shortcut_resolves_to_a_ledger_entry_if_installed() {
        let p = std::path::Path::new(r"C:\Users\lbx13\Desktop\UniGetUI.lnk");
        if !p.exists() {
            println!("跳过：这台机器没有那个 .lnk");
            return;
        }
        let target = crate::lnk::target_of(&std::fs::read(p).unwrap()).unwrap();
        let apps = list_installed().unwrap();
        match match_target(&apps, &target) {
            Some(a) => println!("{} ⇒ 底账里的「{}」({:?})", target, a.name, a.family),
            None => println!(
                "{} ⇒ 底账里没有这条（不判红，装机记录本来就可能缺）",
                target
            ),
        }
    }

    /// 只读地把三棵树真走一遍。这条不是"能跑就行"的冒烟：
    /// 枚举键/枚举值的缓冲区长度、`ERROR_NO_MORE_ITEMS` 的退出条件、
    /// UTF-16 → String 的切法，任何一个错了都会在这里表现为死循环或 panic，
    /// 而纯函数测试完全碰不到它们。
    #[cfg(windows)]
    #[test]
    fn reading_the_real_registry_terminates_and_is_wellformed() {
        let apps = list_installed().expect("读注册表不该失败");
        assert!(
            apps.iter().all(|a| !a.name.trim().is_empty()),
            "每条都该有名字，没名字的应该在 app_from_values 里就被丢掉"
        );
        // 排序由 dedup_by_key 负责；这里确认它真的排了（列表靠"可扫一眼"才有用）
        let names: Vec<String> = apps.iter().map(|a| a.name.to_lowercase()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "底账应按名称排好");
        println!("读到底账 {} 条", apps.len());
    }
}
