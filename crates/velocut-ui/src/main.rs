#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod context;
mod helpers;
mod modules;
mod theme;

fn main() -> eframe::Result {
    ffmpeg_the_third::init().expect("FFmpeg init failed");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("⚡ VeloCut")
            .with_inner_size([1465.0, 925.0])
            .with_min_inner_size([900.0, 600.0])
            .with_decorations(false),
        ..Default::default()
    };

    eframe::run_native(
        "VeloCut",
        native_options,
        Box::new(|cc| Ok(Box::new(app::VeloCutApp::new(cc)))),
    )
}