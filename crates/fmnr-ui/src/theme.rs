use eframe::egui::{self, Color32, CornerRadius, Stroke, Vec2, Visuals};

/// iOS 26 Liquid Glass 令牌 → egui visuals。
/// 诚实边界：egui immediate-mode 无原生 backdrop blur，这里做「半透明层次 + hairline 描边」
/// 近似。窗口级 acrylic 磨砂由平台层（Windows DWM）负责，见 docs/DESIGN.md §5。
pub fn apply_liquid_glass(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    // 玻璃面板：高透明底
    style.visuals.panel_fill = Color32::from_rgba_unmultiplied(255, 255, 255, 180);
    style.visuals.window_fill = Color32::from_rgba_unmultiplied(252, 252, 254, 205);
    style.visuals.extreme_bg_color = Color32::from_rgba_unmultiplied(245, 245, 248, 255);
    style.visuals.faint_bg_color = Color32::from_rgba_unmultiplied(250, 250, 252, 165);

    // hairline 描边
    let hairline = Color32::from_rgba_unmultiplied(0, 0, 0, 20);
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, hairline);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, hairline);

    // iOS 蓝 accent
    let accent = Color32::from_rgb(0, 122, 255);
    style.visuals.selection.bg_fill = accent.linear_multiply(0.28);
    style.visuals.selection.stroke = Stroke::new(1.0_f32, accent);
    style.visuals.hyperlink_color = accent;

    // 胶囊圆角：控件全圆角（egui CornerRadius 是 u8，255 足够表达胶囊）
    let capsule = CornerRadius::same(u8::MAX);
    style.visuals.widgets.noninteractive.corner_radius = capsule;
    style.visuals.widgets.inactive.corner_radius = capsule;
    style.visuals.widgets.hovered.corner_radius = capsule;
    style.visuals.widgets.active.corner_radius = capsule;
    style.visuals.widgets.open.corner_radius = capsule;

    // 间距偏宽松（iOS 风）
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(14.0, 8.0);
    style.spacing.menu_margin = egui::Margin::same(8);

    ctx.set_style(style);
    ctx.set_visuals(Visuals::light());
}
