use eframe::egui::{self, Color32, CornerRadius, FontId, Stroke, TextStyle, Vec2, Visuals};

/// 行内"大小"列的宽度。原来给到 110，名字列和数字之间是一片void；
/// 目录行还填一个「—」，那列对目录本来就没有意义，纯属噪音。
pub const SIZE_COL_W: f32 = 74.0;
/// 图标 + 角标占掉的行首宽度。名字列 = 可用宽 − 这两项，
/// 算在两处就会分叉，所以收成常量。
pub const ROW_LEAD_W: f32 = 26.0;
/// 状态栏左右两段之间的间隔。
pub const STATUS_GAP: f32 = 14.0;

/// 一行的两列宽度：`(名字列, 大小列)`。
///
/// 收成函数是因为这两处以前各写各的字面量（名字列 `avail_w − 130`、
/// 大小列 `110`），改一个忘一个就会让名字和数字重叠或留出一大片空隙。
/// 不变量：两列加起来正好等于可用宽，且不出现负数。
pub fn row_cols(avail_w: f32) -> (f32, f32) {
    let size_w = SIZE_COL_W;
    let name_w = (avail_w - ROW_LEAD_W - size_w).max(60.0);
    (name_w, size_w)
}

/// 用浅色半透明层次和细描边近似 Liquid Glass，不做背景模糊。
pub fn apply_liquid_glass(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = Visuals::light();
    style.override_font_id = None;
    style.override_text_style = None;
    for (role, font) in [
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Heading, FontId::proportional(18.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ] {
        style.text_styles.insert(role, font);
    }

    let accent = Color32::from_rgb(0, 122, 255);
    let text = Color32::from_rgb(36, 41, 51);
    let hairline = Color32::from_rgba_unmultiplied(44, 62, 88, 32);
    let visuals = &mut style.visuals;
    visuals.panel_fill = Color32::from_rgba_unmultiplied(246, 248, 252, 244);
    visuals.window_fill = Color32::from_rgba_unmultiplied(255, 255, 255, 242);
    visuals.extreme_bg_color = Color32::WHITE;
    visuals.faint_bg_color = Color32::from_rgba_unmultiplied(232, 239, 249, 100);
    visuals.code_bg_color = Color32::from_rgb(238, 242, 248);
    visuals.window_stroke = Stroke::new(1.0_f32, hairline);
    visuals.window_corner_radius = CornerRadius::same(14);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(0, 122, 255, 40);
    visuals.selection.stroke = Stroke::new(1.0_f32, text);
    visuals.hyperlink_color = accent;
    visuals.text_cursor.stroke = Stroke::new(2.0_f32, accent);
    visuals.warn_fg_color = Color32::from_rgb(145, 83, 0);
    visuals.error_fg_color = Color32::from_rgb(185, 38, 48);

    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(10);
        widget.fg_stroke = Stroke::new(1.0_f32, text);
        widget.bg_stroke = Stroke::new(1.0_f32, hairline);
        widget.expansion = 0.0;
    }
    // egui 将禁用控件向 noninteractive.weak_bg_fill 淡化。
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(246, 248, 252);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(246, 248, 252);
    visuals.widgets.inactive.bg_fill = Color32::WHITE;
    visuals.widgets.inactive.weak_bg_fill = Color32::from_white_alpha(220);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(235, 244, 255);
    visuals.widgets.hovered.weak_bg_fill = visuals.widgets.hovered.bg_fill;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, accent);
    visuals.widgets.active.bg_fill = Color32::from_rgb(216, 233, 255);
    visuals.widgets.active.weak_bg_fill = visuals.widgets.active.bg_fill;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, accent);
    visuals.widgets.open.bg_fill = Color32::from_rgb(235, 244, 255);
    visuals.widgets.open.weak_bg_fill = visuals.widgets.open.bg_fill;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, accent);

    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(10.0, 6.0);
    style.spacing.interact_size = Vec2::new(32.0, 32.0);
    style.spacing.menu_margin = egui::Margin::same(8);
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liquid_glass_tokens_survive_application() {
        let ctx = egui::Context::default();
        ctx.set_visuals(Visuals::dark());
        apply_liquid_glass(&ctx);
        let style = ctx.style();
        let visuals = &style.visuals;

        assert!(!visuals.dark_mode);
        assert_eq!(visuals.hyperlink_color, Color32::from_rgb(0, 122, 255));
        assert_eq!(
            visuals.widgets.active.bg_stroke,
            Stroke::new(1.0_f32, Color32::from_rgb(0, 122, 255))
        );
        assert_eq!(
            visuals.selection.bg_fill,
            Color32::from_rgba_unmultiplied(0, 122, 255, 40)
        );
        assert_eq!(
            visuals.panel_fill,
            Color32::from_rgba_unmultiplied(246, 248, 252, 244)
        );
        assert_eq!(
            visuals.window_fill,
            Color32::from_rgba_unmultiplied(255, 255, 255, 242)
        );
        assert_eq!(visuals.window_corner_radius, CornerRadius::same(14));
        assert_eq!(visuals.menu_corner_radius, CornerRadius::same(10));
        for widget in [
            &visuals.widgets.noninteractive,
            &visuals.widgets.inactive,
            &visuals.widgets.hovered,
            &visuals.widgets.active,
            &visuals.widgets.open,
        ] {
            assert_eq!(widget.corner_radius, CornerRadius::same(10));
            assert_eq!(widget.fg_stroke.color, Color32::from_rgb(36, 41, 51));
        }
        assert_ne!(visuals.weak_text_color(), visuals.text_color());
        for (role, expected) in [
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Heading, FontId::proportional(18.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ] {
            assert_eq!(style.text_styles.get(&role), Some(&expected));
        }
    }

    /// 行的两列必须**正好铺满**可用宽：以前名字列写 `avail_w − 130`、
    /// 大小列写 `110`，两处各算各的，改一边就出现"名字和数字重叠"或
    /// "中间一片空白"。这条把"加起来等于 avail_w"钉住。
    #[test]
    fn row_columns_tile_the_available_width() {
        for avail in [640.0_f32, 320.0, 120.0] {
            let (name, size) = row_cols(avail);
            assert_eq!(size, SIZE_COL_W, "大小列宽度是定死的列基线");
            assert!(name > 0.0 && name <= avail);
            // 只有窄到名字列触底（60）时才允许铺不满——那时是被 .max(60) 兜住，
            // 不是两列各算各的。
            let sum = ROW_LEAD_W + name + size;
            assert!(
                (sum - avail).abs() < 0.5 || name == 60.0,
                "avail={avail} 两列合计 {sum}"
            );
        }
        // 反证用：极窄时名字列停在 60 而不是变负数。
        assert_eq!(row_cols(40.0).0, 60.0);
    }
}
