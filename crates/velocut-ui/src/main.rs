#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod context;
mod helpers;
mod modules;
mod theme;

fn main() -> eframe::Result {
    ffmpeg_the_third::init().expect("FFmpeg init failed");

    let icon = load_icon();

    let native_options = eframe::NativeOptions {
        centered: true,
        viewport: egui::ViewportBuilder::default()
            .with_title("⚡ VeloCut")
            .with_inner_size([1465.0, 965.0])
            .with_min_inner_size([900.0, 600.0])
            .with_decorations(false)
            .with_resizable(true)
            .with_icon(icon),
        ..Default::default()
    };

    eframe::run_native(
        "VeloCut",
        native_options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(app::VeloCutApp::new(cc)))
        }),
    )
}

fn load_icon() -> egui::IconData {
    let bytes = include_bytes!("../../../assets/linux/icon-256.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("Failed to read icon PNG info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("Failed to get icon buffer size")];
    let info = reader.next_frame(&mut buf).expect("Failed to decode icon PNG");
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => buf[..info.buffer_size()]
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => panic!("Unsupported icon color type"),
    };
    egui::IconData {
        rgba,
        width:  info.width,
        height: info.height,
    }
}