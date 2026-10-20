use eframe::egui;

mod app;
mod board_view;
mod dupes_run;
mod fonts;
mod icons;
mod ops;
mod popwin;
mod preview;
mod search;
mod theme;
mod tree;
mod treemap;
mod usage;
mod win_drop;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("fmnr")
            .with_transparent(true),
        ..Default::default()
    };

    eframe::run_native(
        "fmnr",
        options,
        Box::new(|cc| {
            // 字体要在主题之前：装不上 CJK 的话整个界面文案都是豆腐块，
            // 而这个事实要能落到状态栏上（见 app::App::new 的 cjk_ok）。
            let cjk = fonts::install_cjk_fonts(&cc.egui_ctx);
            if cjk.is_none() {
                tracing::warn!(
                    "未找到任何 CJK 字体：界面里的中文会显示成豆腐块，\
                     候选清单见 fmnr-ui/src/fonts.rs"
                );
            }
            theme::apply_liquid_glass(&cc.egui_ctx);
            Ok(Box::new(app::App::new(cjk.is_some())))
        }),
    )
}
