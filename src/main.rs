#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod media;
mod modules;
mod paths;
mod state;
mod theme;

fn main() -> eframe::Result {
    ffmpeg_the_third::init().expect("FFmpeg init failed");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("⚡ VeloCut")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "VeloCut",
        native_options,
        Box::new(|cc| Ok(Box::new(app::VeloCutApp::new(cc)))),
    )
}