//! 「这个目录是哪个已装程序装的」——**弱**反查。
//!
//! 天花板是量出来的，不是估的（`docs/UI-NEXT.md` §4.4）：本机 297 个 Uninstall 键里
//! 只有 73 个带 `InstallLocation`，其中还有 5 个路径压根不存在 ⇒ 反查上限 ~18%。
//! 所以这个函数只回答"**确证属于谁**"，对不上就说没有，
//! 而且**对上了两个不同的程序也说没有**——它唯一的用途是在目录上打一个角标，
//! 打错角标比不打坏。
//!
//! 另一条硬约束：**不做全量索引**。`D:\rj` 实测 2826 个 exe 要 35 s，
//! 所以只在"人已经进到某个目录"时定点查这一次，不在后台扫。

use crate::registry::InstalledApp;
use crate::Upgrade;
use std::path::{Component, Path};

/// 一条"这个目录里的程序有新版可升"的提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub name: String,
    /// 注册表/winget 报的当前版本
    pub installed: String,
    pub available: String,
    /// 该复制给用户自己跑的命令。**这里不执行**——本项目的立场是只列不代跑。
    pub command: String,
}

/// 目录 → 「确证属于某个程序，**且**那个程序在可升级清单里」。
///
/// 两个条件缺一个就返回 `None`。第二个条件尤其要说清楚为什么可能缺：
/// 可升级清单来自 `winget upgrade`，要几秒且是用户主动查才有的东西，
/// 不能为了打一个角标偷偷去跑它。
///
/// 名字这一道join 只做**大小写不敏感的整串相等**：
/// winget 表里的 Name 与注册表的 DisplayName 通常是同一个串，
/// 而对不上就是对不上——这里宁可不说，也不要指错一个包。
pub fn upgrade_hint(inventory: &[InstalledApp], upgrades: &[Upgrade], dir: &Path) -> Option<Hint> {
    let owner = owner_of_dir(inventory, dir)?;
    let key = owner.name.to_lowercase();
    let row = upgrades
        .iter()
        .find(|u| u.name.to_lowercase() == key && !u.id.trim().is_empty())?;
    Some(Hint {
        name: owner.name.clone(),
        installed: row.installed.clone(),
        available: row.available.clone(),
        command: format!("winget upgrade --id {}", row.id),
    })
}

/// `dir` 的确证归属。`None` 包含三种情况：没有登记项对得上、
/// 对得上的那条登记已经不存在、以及**两个不同程序都声称是它**（歧义不猜）。
pub fn owner_of_dir<'a>(inventory: &'a [InstalledApp], dir: &Path) -> Option<&'a InstalledApp> {
    let target = norm(dir)?;

    // 先挑出"是这段路径的祖先（或就是它自己）"的登记项，再取**最深**的那一层：
    // 嵌套安装时 `…\X` 和 `…\X\Plugins` 可能都在底账里，浅的那条不是答案。
    let mut best: Option<(usize, Vec<&'a InstalledApp>)> = None;
    for app in inventory {
        let Some(loc) = app.install_location.as_deref() else {
            continue;
        };
        let loc = Path::new(loc);
        // 注册表里那条路径可能已经不存在（实测 `TRAE CN` 就是）。
        // 拿一个不存在的目录去判归属，结论只能是猜的。
        if !loc.is_dir() {
            continue;
        }
        let Some(base) = norm(loc) else {
            continue;
        };
        if base.len() > target.len() || target[..base.len()] != base[..] {
            continue;
        }
        match &mut best {
            Some((d, list)) if *d == base.len() => list.push(app),
            Some((d, _)) if *d > base.len() => {}
            _ => best = Some((base.len(), vec![app])),
        }
    }

    let (_, hits) = best?;
    // HKLM / HKCU 各记一条**同一个**程序（名字与版本都一样）不算歧义——
    // 实测这种双份有好几组，一律判歧义就等于把能认的也认不出来。
    let unique: std::collections::BTreeSet<(String, Option<String>)> = hits
        .iter()
        .map(|a| (a.name.to_lowercase(), a.version.clone()))
        .collect();
    if unique.len() != 1 {
        return None;
    }
    hits.into_iter().next()
}

/// 按**路径分量**归一化，用于比较。
///
/// 分量而不是字符串前缀，是因为 `C:\Foo` 是 `C:\Football` 的字符串前缀、
/// 却不是它的父目录：按字符串比会把两个不相干的产品认成同一个，
/// 而这条函数唯一能犯的错就是这个错。
///
/// 只在 Windows 上折大小写——那边文件系统本来不区分，而实测确实有
/// "注册写作 `CD Projekt RED`、磁盘上叫 `CD Projekt Red`"这种对不上的情况。
///
/// 路径里出现 `.`/`..` 就**不判**（返回 None）：没解析过的写法比出来的结果不可信。
fn norm(p: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for c in p.components() {
        let piece = match c {
            Component::Prefix(s) => s.as_os_str().to_string_lossy().to_string(),
            Component::RootDir => "<root>".to_string(),
            Component::Normal(s) => s.to_string_lossy().to_string(),
            // `.` / `..` 以及任何没解析过的写法：比出来的结果不可信，不判。
            _ => return None,
        };
        #[cfg(windows)]
        let piece = piece.to_lowercase();
        out.push(piece);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fmnr-owner-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mk(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        dir.to_path_buf()
    }

    fn app(name: &str, loc: Option<&Path>, version: Option<&str>) -> InstalledApp {
        InstalledApp {
            name: name.into(),
            publisher: None,
            version: version.map(Into::into),
            install_location: loc.map(|p| p.display().to_string()),
            uninstall_string: None,
            quiet_uninstall_string: None,
            url: None,
            hive: crate::registry::Hive::Machine,
            family: crate::registry::Family::Unknown,
            github: None,
            reg_key: None,
        }
    }

    /// 正例两条：进到安装目录里、以及进到它的子目录里，都该认出来。
    #[test]
    fn matches_the_directory_itself_and_anything_below_it() {
        let r = root("hit");
        let app_dir = mk(&r.join("App"));
        let plugins = mk(&app_dir.join("Plugins"));
        let inv = vec![app("SampleApp", Some(&app_dir), Some("1.0"))];

        let got = owner_of_dir(&inv, &app_dir).expect("进到自己目录该认出来");
        assert_eq!(got.name, "SampleApp");
        assert!(
            owner_of_dir(&inv, &plugins).is_some(),
            "子目录也该归给同一个程序"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// 🔴 这条是本函数存在的理由：`App` 是 `Applet` 的**字符串前缀**，
    /// 但不是它的父目录。按字符串前缀比就会把 `Applet` 认成 `App` 装的。
    #[test]
    fn a_sibling_whose_name_starts_with_the_registered_name_is_not_a_match() {
        let r = root("prefix");
        let app_dir = mk(&r.join("App"));
        let sibling = mk(&r.join("Applet"));
        let inv = vec![app("SampleApp", Some(&app_dir), Some("1.0"))];
        assert!(
            owner_of_dir(&inv, &sibling).is_none(),
            "把 Applet 认成 SampleApp 的目录了——这正是角标会打错的那一种"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// 登记的路径已经不存在（实测有这种）：不能拿它判归属。
    #[test]
    fn a_registered_location_that_no_longer_exists_claims_nothing() {
        let r = root("stale");
        let gone = r.join("Gone"); // 故意不创建
        let here = mk(&r.join("Somewhere"));
        let inv = vec![app("Ghost", Some(&gone), Some("1.0"))];
        assert!(owner_of_dir(&inv, &here).is_none());
        let _ = std::fs::remove_dir_all(&r);
    }

    /// 两个**不同**程序都声称同一个目录 → 不猜。
    #[test]
    fn two_different_owners_for_the_same_directory_is_ambiguity() {
        let r = root("ambig");
        let shared = mk(&r.join("Shared"));
        let inv = vec![
            app("Alpha", Some(&shared), Some("1.0")),
            app("Beta", Some(&shared), Some("2.0")),
        ];
        assert!(
            owner_of_dir(&inv, &shared).is_none(),
            "名字都不一样，凭什么说这个目录是谁的"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// HKLM / HKCU 双份的**同一个**程序不算歧义
    /// （实测这种双份有好几组；一律判歧义就把能认的也认不出来了）。
    #[test]
    fn the_same_app_recorded_twice_is_not_ambiguity() {
        let r = root("dup");
        let d = mk(&r.join("Dup"));
        let inv = vec![
            app("Qoder IDE", Some(&d), Some("1.106.3")),
            app("Qoder IDE", Some(&d), Some("1.106.3")),
        ];
        assert!(owner_of_dir(&inv, &d).is_some(), "同名字同版本该算一条");
        let _ = std::fs::remove_dir_all(&r);
    }

    /// 嵌套安装时取**最深**的那条：`…\X\Plugins` 归 Plugins 那个程序，不归 `X`。
    #[test]
    fn the_deepest_registered_location_wins() {
        let r = root("nest");
        let outer = mk(&r.join("X"));
        let inner = mk(&outer.join("Plugins"));
        let inv = vec![
            app("Outer", Some(&outer), Some("1.0")),
            app("Inner", Some(&inner), Some("1.0")),
        ];
        assert_eq!(owner_of_dir(&inv, &inner).unwrap().name, "Inner");
        assert_eq!(owner_of_dir(&inv, &outer).unwrap().name, "Outer");
        let _ = std::fs::remove_dir_all(&r);
    }

    /// 没登记 InstallLocation 的条目（实测 297 条里 224 条没有）不参与判定。
    #[test]
    fn entries_without_a_location_never_match() {
        let r = root("noloc");
        let d = mk(&r.join("Any"));
        let inv = vec![app("NoLocation", None, Some("1.0"))];
        assert!(owner_of_dir(&inv, &d).is_none());
        let _ = std::fs::remove_dir_all(&r);
    }

    /// 带 `..` 的路径不判：比出来的东西不可信。
    #[test]
    fn unresolved_paths_are_not_judged() {
        let r = root("dotdot");
        let app_dir = mk(&r.join("App"));
        let inv = vec![app("SampleApp", Some(&app_dir), Some("1.0"))];
        let dodgy = app_dir.join("..").join("App");
        assert!(
            owner_of_dir(&inv, &dodgy).is_none(),
            "带 .. 的路径该说不知道"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// 注册里的大小写和磁盘上不一样（实测 `CD Projekt RED` vs `CD Projekt Red`）。
    #[cfg(windows)]
    #[test]
    fn case_differences_do_not_hide_the_owner_on_windows() {
        let r = root("case");
        let real = mk(&r.join("CD Projekt Red"));
        let inv = vec![app("RED", Some(&r.join("CD Projekt RED")), Some("1.0"))];
        assert!(
            owner_of_dir(&inv, &real).is_some(),
            "大小写对不上就不认，等于白扔掉一批能认的"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    fn up(name: &str, id: &str, installed: &str, available: &str) -> Upgrade {
        Upgrade {
            name: name.into(),
            id: id.into(),
            installed: installed.into(),
            available: available.into(),
            source: "winget".into(),
        }
    }

    /// 提示要**两个条件都凑齐**：目录确证属于某个程序，且那个程序在可升级清单里。
    /// 只有底账没有清单 ⇒ 不说话（清单要跑 winget，不能为打个角标偷偷跑）。
    #[test]
    fn hint_needs_both_an_owner_and_an_upgrade_row() {
        let r = root("hint");
        let d = mk(&r.join("App"));
        let inv = vec![app("SampleApp", Some(&d), Some("1.0"))];

        let got = upgrade_hint(&inv, &[up("SampleApp", "vendor.sample", "1.0", "2.0")], &d)
            .expect("两个条件都满足，该提示");
        assert_eq!(got.available, "2.0");
        assert_eq!(
            got.command, "winget upgrade --id vendor.sample",
            "给的是**让用户自己跑**的命令，不是我们代跑"
        );

        assert!(
            upgrade_hint(&inv, &[], &d).is_none(),
            "没查过更新也要闭嘴，不能为了提示去偷偷跑 winget"
        );
        let other = mk(&r.join("Other"));
        assert!(
            upgrade_hint(
                &inv,
                &[up("SampleApp", "vendor.sample", "1.0", "2.0")],
                &other
            )
            .is_none(),
            "不属于任何程序的目录不该有提示"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// join 只做"大小写不敏感的整串相等"。
    /// 半句"顺手用前缀匹配"是这里最贵的错：`SampleApp` 会去认领 `SampleApp Beta` 的升级，
    /// 然后提示用户升错包。
    #[test]
    fn the_name_join_is_whole_string_not_a_substring() {
        let r = root("join");
        let d = mk(&r.join("App"));
        let inv = vec![app("SampleApp", Some(&d), Some("1.0"))];
        assert!(
            upgrade_hint(&inv, &[up("SAMPLEAPP", "vendor.sample", "1.0", "2.0")], &d).is_some(),
            "大小写不同该算同一个名字"
        );
        assert!(
            upgrade_hint(
                &inv,
                &[up("SampleApp Beta", "vendor.sample.beta", "1.0", "2.0")],
                &d
            )
            .is_none(),
            "前缀相同不是同一个程序，指错包比不指危害大"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// **拿真注册表跑一遍自洽性**（只在 Windows 上有底账可读）。
    ///
    /// 断言不依赖这台机器装了什么：凡是登记了安装位置**且那个目录还在**的条目，
    /// 把它自己的安装位置喂回去，就必须认出它自己（或它自己在另一个 hive 里的那条）。
    /// 这条抓的是"真实数据形状"那一类错——尾斜杠、`(x86)` 里的括号、中日韩名字、
    /// 大小写不一致、HKLM/HKCU 双份——手写夹具容易恰好避开它们。
    #[cfg(windows)]
    #[test]
    fn every_real_registration_recognises_its_own_directory() {
        let inv = crate::registry::list_installed().expect("读本机底账");
        let mut checked = 0usize;
        let mut refused = 0usize;
        for a in &inv {
            let Some(loc) = a.install_location.as_deref() else {
                continue;
            };
            let p = std::path::Path::new(loc);
            if !p.is_dir() {
                continue; // 登记的路径已经没了，这种本来就不该认
            }
            if let Some(hit) = owner_of_dir(&inv, p) {
                // 认出来了就必须是它自己。认成别的程序 = 角标打错，是最坏的一种。
                assert_eq!(
                    hit.name.to_lowercase(),
                    a.name.to_lowercase(),
                    "{loc} 该归 {}，实归 {}",
                    a.name,
                    hit.name
                );
                checked += 1;
            } else {
                refused += 1;
            }
        }
        assert!(
            checked > 0,
            "一条都没判出来——那说明这条测试什么都没测（本机底账 {} 条）",
            inv.len()
        );
        println!("真底账：认出自己目录 {checked} 条，按\"宁漏勿错\"主动放弃 {refused} 条");
    }

    /// 真数据里"登记了但目录已经不存在"的条目，不能把那个路径指向**别的**程序。
    /// 不数具体个数（换台机器就变），只钉这个性质。
    #[cfg(windows)]
    #[test]
    fn a_stale_registration_never_points_at_somebody_else() {
        let inv = crate::registry::list_installed().expect("读本机底账");
        for a in &inv {
            let Some(loc) = a.install_location.as_deref() else {
                continue;
            };
            let p = std::path::Path::new(loc);
            if p.is_dir() {
                continue;
            }
            if let Some(hit) = owner_of_dir(&inv, p) {
                assert_eq!(
                    hit.name.to_lowercase(),
                    a.name.to_lowercase(),
                    "不存在的路径 {loc} 被判成了 {} 的目录",
                    hit.name
                );
            }
        }
    }
}
