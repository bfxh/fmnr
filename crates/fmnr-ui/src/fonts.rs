//! 安装中日韩字体。
//!
//! 🔴 **为什么必须有这个文件**：egui 的 `default_fonts` 只带 Latin（Ubuntu-Light /
//! Hack）和 Noto Emoji，**不含任何 CJK 字形**。本 UI 的文案几乎全是中文
//! （「搜索」「白板」「钉到白板」「隐藏项」「排序」…），不装字体的话
//! 它们全部渲染成豆腐块 `□`。这个缺陷**单测永远抓不到**——它是像素层面的事实，
//! 只有把窗口真开起来截图才看得见（2026-10-16 首次实跑 UI 时撞见）。
//!
//! 选型注意：
//! - 优先 **静态 `.ttf`**。`.ttc`（`msyh.ttc` / `simsun.ttc`）是字体集合，
//!   egui 底层的 ab_glyph 对集合的支持不可靠；可变字体（`*-VF.ttf`）同样有风险。
//! - 装不上就**降级继续跑**（只剩 Latin），绝不 panic：字体文件在精简镜像上
//!   可能整个不存在，为一个字体把文件管理器打死是本末倒置。
//! - 字体**追加为族尾回退**，不替换默认字体——Latin 仍走 egui 自带那套，
//!   免得换字体把既有排版度量全改了。

use eframe::egui::{self, FontData, FontFamily};
use std::path::Path;

/// 候选字体路径，按优先级排。顺序即理由。
pub const CJK_CANDIDATES: &[&str] = &[
    // Windows：黑体，静态 ttf，CJK 覆盖完整。
    r"C:\Windows\Fonts\simhei.ttf",
    // Windows：等线，静态 ttf（Vista+ 随附），比黑体更贴现代 UI。
    r"C:\Windows\Fonts\Deng.ttf",
    // Linux：Noto CJK 常见落点（ttc 集合，ab_glyph 支持有限，故排在 Windows 之后）。
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/arphic/uming.ttc",
    // macOS：PingFang 也在 .ttc 里，同样只当兜底。
    "/System/Library/Fonts/PingFang.ttc",
];

/// 注册表里用的字体名。
pub const CJK_FONT_KEY: &str = "cjk";

/// 把一份字体数据挂成两个族的**尾部回退**。
///
/// 拆成纯函数是为了能被判红：`install_cjk_fonts` 要读真实字体文件，
/// 在没装 CJK 的机器上直接返回 None，测不到「挂没挂上」这件事。
/// 挂错名字（`font_data` 的 key 与族里的字符串对不上）egui **不报错**，
/// 只是静默继续用默认字体 ⇒ 中文照样豆腐块。所以这条必须被钉住。
pub fn add_cjk_fallback(fonts: &mut egui::FontDefinitions, data: Vec<u8>) {
    // epaint 的字段是 `BTreeMap<String, Arc<FontData>>` —— 少一层 Arc 就编不过。
    fonts.font_data.insert(
        CJK_FONT_KEY.to_owned(),
        std::sync::Arc::new(FontData::from_owned(data)),
    );

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        if !list.iter().any(|s| s == CJK_FONT_KEY) {
            list.push(CJK_FONT_KEY.to_owned());
        }
    }
}

/// 找到本机第一个可用的 CJK 字体路径。
pub fn find_cjk_font() -> Option<&'static Path> {
    CJK_CANDIDATES.iter().map(Path::new).find(|p| p.is_file())
}

/// 给 ctx 装上能找到的第一个 CJK 字体。
///
/// 返回 `Some(路径)` 表示装上了；`None` 表示一个都没找到，
/// 此时 UI 会以豆腐块显示中文——调用方应把这件事显示在状态栏里，别静默。
pub fn install_cjk_fonts(ctx: &egui::Context) -> Option<&'static Path> {
    let path = find_cjk_font()?;
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("读 {} 失败，中文将以豆腐块显示：{e}", path.display());
            return None;
        }
    };
    let mut fonts = egui::FontDefinitions::default();
    add_cjk_fallback(&mut fonts, bytes);
    ctx.set_fonts(fonts);
    tracing::info!("已安装 CJK 字体：{}", path.display());
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 候选必须**根化**（不依赖 CWD）。
    ///
    /// 🔴 刻意不用 `Path::is_absolute()`：那是**平台相对**的判据——
    /// 在 Windows 上 `/usr/share/fonts/...` 返回 false（它要盘符或 UNC），
    /// 于是这条测试会变成「只在 Unix 上过」的假判据。
    /// 我真正要钉的是「不许出现相对路径」，那就直接查首字符。
    #[test]
    fn candidates_are_non_empty_and_rooted() {
        assert!(!CJK_CANDIDATES.is_empty());
        for p in CJK_CANDIDATES {
            assert!(
                p.starts_with('/') || looks_like_windows_root(p),
                "候选必须根化，不能依赖 CWD：{p}"
            );
        }
    }

    /// Windows 形态的根：盘符 `C:\` 或 UNC `\\server\share`。
    fn looks_like_windows_root(p: &str) -> bool {
        let b = p.as_bytes();
        let drive = b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/');
        let unc = p.starts_with(r"\\");
        drive || unc
    }

    /// 首选必须是**静态 ttf**：`.ttc` 集合与可变字体对 ab_glyph 有风险。
    /// 这条钉住选型，免得有人把 `msyh.ttc` 提到第一位还自觉更"原生"。
    #[test]
    fn first_candidate_is_static_ttf() {
        let first = CJK_CANDIDATES[0];
        assert!(first.ends_with(".ttf"), "首选应是静态 ttf：{first}");
        assert!(!first.contains("-VF"), "可变字体不当首选：{first}");
    }

    /// 核心判据：挂上去之后，两个族的**末尾**必须是那个已注册的 key。
    /// 族里有名字但 `font_data` 里没有 = egui 静默忽略 = 照样豆腐块，
    /// 所以两边都要查，而且要查同一个字符串。
    #[test]
    fn cjk_is_registered_and_appended_to_both_families() {
        let mut fonts = egui::FontDefinitions::default();
        add_cjk_fallback(&mut fonts, vec![1, 2, 3, 4]);

        assert!(
            fonts.font_data.contains_key(CJK_FONT_KEY),
            "font_data 里没有 {CJK_FONT_KEY}"
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let list = fonts
                .families
                .get(&family)
                .unwrap_or_else(|| panic!("{family:?} 族不见了"));
            assert_eq!(
                list.last().map(String::as_str),
                Some(CJK_FONT_KEY),
                "{family:?} 的尾部回退不是 {CJK_FONT_KEY}：{list:?}"
            );
        }
    }

    /// 幂等：重复安装不该在族里塞出两份，否则回退顺序会漂。
    #[test]
    fn applying_twice_does_not_duplicate() {
        let mut fonts = egui::FontDefinitions::default();
        add_cjk_fallback(&mut fonts, vec![0]);
        add_cjk_fallback(&mut fonts, vec![0]);
        let list = fonts.families.get(&FontFamily::Proportional).unwrap();
        assert_eq!(
            list.iter().filter(|s| s.as_str() == CJK_FONT_KEY).count(),
            1,
            "{CJK_FONT_KEY} 被挂了多次：{list:?}"
        );
    }

    /// Latin 必须**留在前面**——把 CJK 插到族首会让中文优先、拉丁也改用黑体，
    /// 等于悄悄换掉了整套既有排版度量。
    #[test]
    fn latin_fonts_are_kept_ahead() {
        let mut fonts = egui::FontDefinitions::default();
        let before = fonts
            .families
            .get(&FontFamily::Proportional)
            .unwrap()
            .clone();
        assert!(!before.is_empty(), "egui 默认就该有 proportional 族");
        add_cjk_fallback(&mut fonts, vec![0]);
        let after = fonts.families.get(&FontFamily::Proportional).unwrap();
        assert_eq!(
            &after[..before.len()],
            &before[..],
            "原有的拉丁字体顺序被动了"
        );
        assert_eq!(after.len(), before.len() + 1, "只该多出一个尾部回退");
    }

    /// 真跑一遍安装。本机（Windows）应命中；命中就必须同时保证
    /// 「文件非空」与「ctx 不 panic」。没装的机器走降级，打印说明。
    #[test]
    fn install_on_this_machine_either_loads_or_degrades() {
        let ctx = egui::Context::default();
        match install_cjk_fonts(&ctx) {
            Some(p) => {
                let n = std::fs::read(p).expect("命中的候选应可读").len();
                assert!(n > 1024, "字体文件小得不像话：{} = {n} 字节", p.display());
            }
            None => eprintln!("本机未命中任何 CJK 字体 ⇒ 走降级（中文会显示为豆腐块）"),
        }
    }
}
