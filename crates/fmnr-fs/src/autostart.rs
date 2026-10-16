//! 开机自启动：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 里我们自己的那一个值。
//!
//! DESIGN §111 原本只承认两个碰系统边界的动作（注册默认程序、spawn 外部程序）。
//! 自启动是**第三个**，而且是用户明确要的功能——所以这里把它写成和
//! `assoc` 一样的形状，并同步改了 DESIGN 那句话，不让文档停留在"只有两个"。
//!
//! 三条约束继承自 DESIGN 第 68 行的「最小侵入」：
//!
//! 1. **只写 HKCU**。免管理员、只影响当前用户、用户自己能在任务管理器里关掉。
//! 2. **`Run` 是共享键**：里面是这台机器上所有程序的自启动项。所以它进
//!    `RegistrationPlan::shared_values` 而**不进** `owned_keys`——撤销时只删
//!    `fmnr` 这一个值，绝不删键。删键等于把别人的自启动一起清空。
//! 3. **看得见别人**：如果那个值名被别的程序占了（或用户改成了别的命令），
//!    报 `Foreign` 而不是静默覆盖。

use crate::assoc::{self, RegistrationPlan};
use fmnr_core::Result;

/// `HKCU` 下的相对路径（不带 hive 前缀——`assoc` 的写入函数就是按 HKCU 相对路径设计的）。
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 我们占用的值名。改名等于留下孤儿项，所以它是常量而不是参数。
pub const VALUE_NAME: &str = "fmnr";

/// 自启动计划：一条写入 + 一条"只删自己那个值"的撤销路径。
///
/// 刻意复用 `RegistrationPlan`：`apply`/`revert` 已经把「共享键只删值、
/// 独占键才整棵删」这条区分实现并测过了，这里再造一套注册表写入 plumbing
/// 只会多一处能写错的地方。
pub fn plan(exe: &str) -> RegistrationPlan {
    RegistrationPlan {
        ops: vec![assoc::RegOp {
            subkey: RUN_KEY.to_string(),
            name: VALUE_NAME.to_string(),
            // Run 里的命令行要带引号：exe 路径含空格时（`D:\Program Files\…`）
            // 不加引号会被切成两个参数，开机静默启不来。
            value: assoc::RegValue::Sz(format!("\"{exe}\"")),
        }],
        // 空 = 我们没有独占任何键，因此 revert 不会去删任何键。
        owned_keys: Vec::new(),
        shared_values: vec![(RUN_KEY.to_string(), VALUE_NAME.to_string())],
    }
}

/// 那个值现在是什么状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Run 里没有我们这一项
    Off,
    /// 有，且指向这个 exe
    Ours,
    /// 有，但指向别处——**不能覆盖**，要交给用户判断
    Foreign(String),
}

/// 纯函数：把"读到的值"和"我们认为是自己的 exe"分类。
///
/// 比较时把两边都按原样比（Run 里存的就是带引号的串），因为自启动路径的
/// 规范化（大小写、`\\?\` 前缀、8.3 短名）不是这里该猜的事；判不准就报
/// `Foreign`，宁可多问一句也不要静默改写别人的配置。
pub fn classify(current: Option<&str>, exe: &str) -> Status {
    match current {
        None => Status::Off,
        Some(v) => {
            if v == format!("\"{exe}\"") {
                Status::Ours
            } else {
                Status::Foreign(v.to_string())
            }
        }
    }
}

/// 打开自启动。
pub fn enable(exe: &str) -> Result<assoc::RegistrationReport> {
    assoc::apply(&plan(exe))
}

/// 关掉自启动：只删 `Run` 下的 `fmnr` 值。
pub fn disable() -> Result<assoc::RegistrationReport> {
    assoc::revert(&plan(""))
}

/// 读当前状态。非 Windows 返回 `Unsupported`（不静默当成"没开"）。
pub fn status(exe: &str) -> Result<Status> {
    Ok(classify(current_command()?.as_deref(), exe))
}

/// 读 `Run\fmnr` 的原值。
#[cfg(windows)]
pub fn current_command() -> Result<Option<String>> {
    read_hkcu_value(RUN_KEY, VALUE_NAME)
}

#[cfg(not(windows))]
pub fn current_command() -> Result<Option<String>> {
    Err(crate::Error::Unsupported(
        "自启动状态目前只实现了 Windows（HKCU\\...\\Run）".into(),
    ))
}

/// 读一个 HKCU 下的 REG_SZ / REG_EXPAND_SZ 值；不存在时 `Ok(None)`。
#[cfg(windows)]
fn read_hkcu_value(subkey: &str, name: &str) -> Result<Option<String>> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };

    let sub = crate::assoc::wide(subkey);
    let val = crate::assoc::wide(name);
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;

    // 先问长度（pvdata 传 None），再按长度取。
    let mut len: u32 = 0;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sub.as_ptr()),
            windows::core::PCWSTR(val.as_ptr()),
            flags,
            None,
            None,
            Some(&mut len),
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != ERROR_SUCCESS {
        return Err(crate::Error::Other(format!(
            "RegGetValueW({subkey}\\{name}) 问长度失败：{rc:?}"
        )));
    }

    let mut buf: Vec<u16> = vec![0; (len as usize).div_ceil(2) + 1];
    let mut out_len = len;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sub.as_ptr()),
            windows::core::PCWSTR(val.as_ptr()),
            flags,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut out_len),
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != ERROR_SUCCESS {
        return Err(crate::Error::Other(format!(
            "RegGetValueW({subkey}\\{name}) 失败：{rc:?}"
        )));
    }
    let chars = (out_len as usize).div_ceil(2).min(buf.len());
    Ok(Some(
        String::from_utf16_lossy(&buf[..chars])
            .trim_end_matches('\0')
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这条是**安全不变量**，不是形状检查：`Run` 下装着这台机器所有程序的
    /// 自启动项，一旦它进 `owned_keys`，`revert` 就会整键删掉，
    /// 把别人的自启动一起清空。所以计划必须只有 shared_values、owned_keys 恒空。
    #[test]
    fn run_key_is_shared_never_owned() {
        let p = plan(r"D:\Program Files\fmnr\fmnr-ui.exe");
        assert_eq!(p.ops.len(), 1, "只写一个值");
        assert!(
            p.owned_keys.is_empty(),
            "自启动计划绝不能声明任何独占键，否则撤销会删掉整个 Run 键：{:?}",
            p.owned_keys
        );
        assert_eq!(
            p.shared_values,
            vec![(RUN_KEY.to_string(), VALUE_NAME.to_string())]
        );
    }

    #[test]
    fn plan_stays_inside_hkcu() {
        let p = plan("C:\\x\\fmnr-ui.exe");
        for (key, _) in p.shared() {
            let upper = key.to_uppercase();
            assert!(
                !upper.starts_with("HKLM") && !upper.starts_with("HKEY_LOCAL_MACHINE"),
                "计划里出现了机器级键：{key}"
            );
            assert!(
                !upper.starts_with("HKEY_"),
                "键路径应是 HKCU 下的相对路径，不该自带 hive 前缀：{key}"
            );
        }
    }

    /// exe 路径含空格时不带引号，Windows 会把 `Files\fmnr-ui.exe` 当第二个参数，
    /// 结果是"设置成功了但开机启不来"——最难查的那种。
    #[test]
    fn command_is_quoted_for_paths_with_spaces() {
        let p = plan(r"D:\Program Files\fmnr\fmnr-ui.exe");
        let assoc::RegValue::Sz(v) = &p.ops[0].value else {
            panic!("自启动值应是 REG_SZ");
        };
        assert_eq!(v, r#""D:\Program Files\fmnr\fmnr-ui.exe""#);
    }

    #[test]
    fn status_classifies_three_ways() {
        let exe = r"D:\fmnr\fmnr-ui.exe";
        assert_eq!(classify(None, exe), Status::Off);
        assert_eq!(
            classify(Some(r#""D:\fmnr\fmnr-ui.exe""#), exe),
            Status::Ours
        );
        // 值名撞车（或用户自己改过）⇒ Foreign，不能覆盖
        assert_eq!(
            classify(Some(r#""C:\other\thing.exe""#), exe),
            Status::Foreign(r#""C:\other\thing.exe""#.into())
        );
    }

    /// 真机往返：默认跳过，`FMNR_AUTOSTART_LIVE=1` 才跑。
    /// 它只加/删 `Run\fmnr` 这一个值，不碰别人的项。
    #[cfg(windows)]
    #[test]
    fn live_roundtrip_only_touches_our_value() {
        if std::env::var("FMNR_AUTOSTART_LIVE").is_err() {
            println!("跳过：设 FMNR_AUTOSTART_LIVE=1 才跑真注册表往返");
            return;
        }
        let exe = r"E:\tmp\fmnr-autostart-probe.exe";
        let before = current_command().unwrap();
        enable(exe).unwrap();
        assert_eq!(status(exe).unwrap(), Status::Ours);
        disable().unwrap();
        assert_eq!(
            current_command().unwrap(),
            before,
            "撤销后要回到动手前的状态"
        );
    }
}
