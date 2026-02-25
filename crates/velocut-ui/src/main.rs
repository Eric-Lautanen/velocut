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
            .with_title("VeloCut")
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
            // install_image_loaders is called here only — removed the duplicate
            // call that was inside VeloCutApp::new().
            egui_extras::install_image_loaders(&cc.egui_ctx);
            // NOTE: do NOT set reduce_texture_memory = true here.
            // It causes +20 MB idle overhead for scrub/playback workloads that
            // re-upload textures every frame. See SPEEDRUNAI.md invariants.
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

/// On Windows, borderless windows (`with_decorations(false)`) are created with
/// `WS_POPUP` style which does NOT receive `WS_EX_APPWINDOW` automatically.
/// Without it the shell either omits the taskbar button or shows it without
/// the app icon.
///
/// This function enumerates all windows on the calling thread (the UI/main
/// thread) and ORs WS_EX_APPWINDOW into each one's extended style. Because
/// eframe creates exactly one window on the UI thread, this always hits the
/// right window with zero ambiguity.
///
/// No extra crate dependency — user32.dll and kernel32.dll are always linked
/// on Windows builds. No raw_window_handle import needed at all.
#[cfg(target_os = "windows")]
pub fn fix_taskbar_icon() {
    const GWL_EXSTYLE:     i32   = -20;
    const GCL_HICON:       i32   = -14; // large icon stored on the window class by eframe
    const GCL_HICONSM:     i32   = -34; // small icon stored on the window class by eframe
    const WS_EX_APPWINDOW: isize = 0x0004_0000;
    const WM_SETICON:       u32  = 0x0080;
    const ICON_SMALL:      usize = 0;
    const ICON_BIG:        usize = 1;

    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn EnumThreadWindows(
            thread_id: u32,
            callback:  unsafe extern "system" fn(isize, isize) -> i32,
            param:     isize,
        ) -> i32;
        fn GetWindowLongPtrW(hwnd: isize, n_index: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: isize, n_index: i32, new_val: isize) -> isize;
        fn GetClassLongPtrW(hwnd: isize, n_index: i32) -> usize;
        fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    // Callback invoked by EnumThreadWindows for each window on the thread.
    // 1. Patches WS_EX_APPWINDOW so the shell gives us a taskbar button.
    // 2. Reads the HICON that eframe stored on the window *class* (via with_icon())
    //    and sends WM_SETICON to bind it to the window *instance* explicitly.
    //    Without step 2, WS_POPUP windows show the generic exe icon in the taskbar
    //    even though the class icon is set correctly.
    unsafe extern "system" fn patch_one(hwnd: isize, _param: isize) -> i32 {
        // Step 1 — extended style
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_APPWINDOW);

        // Step 2 — propagate class icons to the window instance so the taskbar
        //           and alt-tab switcher display the correct icon.
        let hicon_big = GetClassLongPtrW(hwnd, GCL_HICON);
        if hicon_big != 0 {
            SendMessageW(hwnd, WM_SETICON, ICON_BIG, hicon_big as isize);
        }
        let hicon_sm = GetClassLongPtrW(hwnd, GCL_HICONSM);
        if hicon_sm != 0 {
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL, hicon_sm as isize);
        }

        1 // non-zero = continue enumeration
    }

    unsafe {
        EnumThreadWindows(GetCurrentThreadId(), patch_one, 0);
    }
}

#[cfg(not(target_os = "windows"))]
pub fn fix_taskbar_icon() {}