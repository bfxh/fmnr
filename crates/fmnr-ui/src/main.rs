use eframe::egui;

mod app;
mod board_view;
mod icons;
mod ops;
mod preview;
mod search;
mod theme;

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
            theme::apply_liquid_glass(&cc.egui_ctx);
            Ok(Box::new(app::App::new()))
        }),
    )
}
