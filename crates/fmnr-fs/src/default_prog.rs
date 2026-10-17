//! 系统默认程序的**读取**（出向：只查不改）。
//!
//! 为什么单独一个模块而不是塞进 `assoc.rs`：那个文件管的是"把 fmnr 登记成
//! 候选程序"（写 HKCU），这里管的是"用户自己定的默认程序是谁"（读 HKCU/HKLM）。
//! 一个是会改系统的，一个只读——混在一起容易把"我只查了一下"写成"我改了设置"。
//!
//! 边界（DESIGN 第 68 行「最小侵入」）：这里**只读不写**，绝不改任何默认值；
//! 解析出来也只是交给 `CreateProcessW` 去 spawn（DESIGN 第 198 行要求的路径），
//! 不走 `ShellExecute`。

use fmnr_core::Result;
use std::path::Path;

/// 一条 `shell\open\command` 解析出来的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultOpen {
    /// 注册表里写的可执行文件（**未必是绝对路径**，可能是 `notepad.exe` 这种要搜 PATH 的）
    pub program: String,
    /// 去掉 `%1`/`%*` 之后的参数模板。文件路径由调用方补，
    /// 这样"默认程序"和"显式指定程序"两条路共用同一套拼接代码。
    pub args: String,
}

/// 从 `shell\open\command` 的命令行里取出「程序 + 去掉占位符的参数」。
///
/// 纯函数，跨平台可测。占位符按 Windows 的写法处理：
/// `%1` 是文件、`%*` 是"其余参数"（文档类型处理器用），两者都要摘掉——
/// 留着 `%1` 会让文件被传两次，留着 `%*` 会 spawn 出一个等参数的壳。
pub fn parse_open_command(cmd: &str) -> Option<DefaultOpen> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    // 程序部分：引号包住的整段，或第一个空格前的裸路径
    let (program, rest) = if let Some(after) = cmd.strip_prefix('"') {
        let end = after.find('"')?;
        (after[..end].to_string(), after[end + 1..].trim())
    } else {
        match cmd.split_once(' ') {
            Some((p, r)) => (p.to_string(), r.trim()),
            None => (cmd.to_string(), ""),
        }
    };
    if program.is_empty() {
        return None;
    }
    let args = drop_placeholders(rest);
    Some(DefaultOpen { program, args })
}

/// 把参数串里的 `%1`（含被引号包住的 `"%1"`）与 `%*` 摘掉，并压掉多余空格。
fn drop_placeholders(args: &str) -> String {
    let mut out = Vec::new();
    // 先按空格切 token（引号内的空格算同一个 token），再逐 token 判占位符
    let mut cur = String::new();
    let mut quoted = false;
    for c in args.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ' ' | '\t' if !quoted => {
                if !cur.is_empty() {
                    out.push(cur.clone());
                    cur.clear();
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.retain(|t| {
        let bare = t.trim_matches('"');
        !(bare.eq_ignore_ascii_case("%1") || bare.eq_ignore_ascii_case("%*") || bare.is_empty())
    });
    out.join(" ")
}

/// 该用哪个扩展名去查默认程序。没有扩展名时用 `*`（Windows 的"未知类型"）。
fn ext_key(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) if !e.is_empty() => format!(".{}", e.to_ascii_lowercase()),
        _ => "*".to_string(),
    }
}

/// 查一个文件的系统默认程序。
///
/// 顺序照 Windows 自己的解析次序，前三步是可测的纯数据：
/// 1. `HKCU\...\Explorer\FileExts\<ext>\UserChoice` 的 `ProgID` —— 用户在
///    "打开方式"里点过"始终"之后的权威值；
/// 2. `HKCU\Software\Classes\<ext>` 的默认值 —— 当前用户的扩展名映射；
/// 3. `HKLM\Software\Classes\<ext>` 的默认值 —— 机器级映射；
/// 4. 拿到 ProgID 后再读 `<ProgID>\shell\open\command`（先 HKCU 后 HKLM）。
///
/// 查不到返回 `Ok(None)` 而不是报错：没有默认程序是一种正常状态（新装的机器、
/// 精简过的系统），调用方要能把它渲染成一句人话而不是一个异常。
pub fn resolve_for(path: &Path) -> Result<Option<DefaultOpen>> {
    // 扩展名这一步放在**两侧都跑**的公共路径上：它只依赖路径本身，
    // 而且如果只给 Windows 分支用，非 Windows 上它就成死代码
    // （门禁第 5 步正是为这类漏网准备的）。
    inner(&ext_key(path))
}

/// 查不到一律 `Ok(None)`（"这台机器没给它配默认程序"是正常状态，不是异常）。
#[cfg(windows)]
fn inner(ext: &str) -> Result<Option<DefaultOpen>> {
    let Some(progid) = user_choice_progid(ext).or_else(|| class_progid(ext)) else {
        return Ok(None);
    };
    let Some(cmd) = progid_open_command(&progid)? else {
        return Ok(None);
    };
    Ok(parse_open_command(&cmd))
}

/// 非 Windows：不解析。调用方拿到 `None` 会去报"没配默认程序"，
/// 而不是静默起一个别的进程。
#[cfg(not(windows))]
fn inner(_ext: &str) -> Result<Option<DefaultOpen>> {
    Ok(None)
}

/// 注册表里那些键名（纯数据，跨平台可测——**不**碰注册表）。
pub mod keys {
    /// 用户"始终用这个打开"的选择落在这儿。
    pub fn user_choice(ext: &str) -> String {
        format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext}\UserChoice")
    }
    pub fn classes(ext: &str) -> String {
        format!(r"Software\Classes\{ext}")
    }
    pub fn open_command(progid: &str) -> String {
        format!(r"Software\Classes\{progid}\shell\open\command")
    }
}

#[cfg(windows)]
fn user_choice_progid(ext: &str) -> Option<String> {
    crate::assoc::get_hkcu_str(&keys::user_choice(ext), "ProgID")
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
}

#[cfg(windows)]
fn class_progid(ext: &str) -> Option<String> {
    // 默认值（值名为空串）：先当前用户，再机器级
    crate::assoc::get_hkcu_str(&keys::classes(ext), "")
        .ok()
        .flatten()
        .or_else(|| {
            crate::assoc::get_hklm_str(&keys::classes(ext), "")
                .ok()
                .flatten()
        })
        .filter(|s| !s.trim().is_empty())
}

#[cfg(windows)]
fn progid_open_command(progid: &str) -> Result<Option<String>> {
    let sub = keys::open_command(progid);
    Ok(crate::assoc::get_hkcu_str(&sub, "")
        .ok()
        .flatten()
        .or_else(|| crate::assoc::get_hklm_str(&sub, "").ok().flatten())
        .filter(|s| !s.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_program_with_spaces_and_placeholder() {
        let d = parse_open_command(r#""C:\Program Files\LaTeX\LaTeX.exe" "%1""#).unwrap();
        assert_eq!(d.program, r"C:\Program Files\LaTeX\LaTeX.exe");
        assert_eq!(
            d.args, "",
            "%1 必须摘干净，否则文件会被传两次：{:?}",
            d.args
        );
    }

    #[test]
    fn extra_args_survive_but_placeholders_go() {
        let d =
            parse_open_command(r#""C:\py\python.exe" "C:\py\script.py" "%1" --view %*"#).unwrap();
        assert_eq!(d.program, r"C:\py\python.exe");
        assert_eq!(d.args, r#""C:\py\script.py" --view"#, "实得 {:?}", d.args);
    }

    #[test]
    fn bare_program_without_quotes() {
        let d = parse_open_command("notepad.exe %1").unwrap();
        assert_eq!(d.program, "notepad.exe");
        assert_eq!(d.args, "");
    }

    #[test]
    fn empty_and_garbage_return_none_not_panic() {
        assert!(parse_open_command("").is_none());
        assert!(parse_open_command("   ").is_none());
        // 只有开引号没有闭引号：不猜，判 None
        assert!(parse_open_command("\"C:\\unclosed.exe %1").is_none());
    }

    #[test]
    fn extension_key_is_lowercased_and_dotted() {
        assert_eq!(ext_key(Path::new(r"C:\a\B.TXt")), ".txt");
        assert_eq!(ext_key(Path::new(r"C:\a\noext")), "*");
        // `.gitignore` 这类点文件在 Rust 与 Windows 眼里都**没有**扩展名
        // （`Path::extension()` 返回 None），所以走"未知类型"的 `*`
        assert_eq!(ext_key(Path::new(".gitignore")), "*");
    }

    /// 键名拼错一位就是"查了个不存在的键→永远查不到"，而且不报错。
    /// 所以这几串钉死到字面。
    #[test]
    fn registry_key_shapes_are_pinned() {
        assert_eq!(
            keys::user_choice(".txt"),
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.txt\UserChoice"
        );
        assert_eq!(keys::classes(".txt"), r"Software\Classes\.txt");
        assert_eq!(
            keys::open_command("AppX43hnxtbyyps62jhe9sqpdzxn1790zetc"),
            r"Software\Classes\AppX43hnxtbyyps62jhe9sqpdzxn1790zetc\shell\open\command"
        );
    }

    /// 真机读数：默认程序解析必须在这台 Windows 上真的拿到一个存在的 exe。
    /// 拿不到就跳过（CI 的 windows runner 上扩展名映射可能不全），
    /// 但**拿到了就必须是个真文件**——这是"解析对了"的唯一硬判据。
    #[cfg(windows)]
    #[test]
    fn live_lookup_on_this_machine_resolves_to_a_real_exe() {
        let probe = std::env::temp_dir().join("fmnr-default-probe.txt");
        std::fs::write(&probe, b"x").unwrap();
        let got = resolve_for(&probe).expect("注册表读取不该报错");
        let _ = std::fs::remove_file(&probe);
        let Some(d) = got else {
            eprintln!("跳过：这台机器没给 .txt 配默认程序");
            return;
        };
        let exe = crate::resolve_program(&d.program)
            .unwrap_or_else(|| panic!("解析出的程序找不到：{d:?}"));
        assert!(
            exe.is_absolute() && exe.exists(),
            "默认程序必须落到一个真实存在的 exe，实得 {exe:?}（注册表原值 {d:?}）"
        );
        assert!(
            !d.args.contains("%1"),
            "参数模板里不该还留着 %1：{:?}",
            d.args
        );
    }
}
