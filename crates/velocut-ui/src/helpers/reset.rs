// src/helpers/reset.rs
//
// Filesystem cleanup and post-reset UI for the VeloCut reset flow.
//
// VeloCut is a standalone .exe with no installer or uninstall script. The
// "Reset" button in the export panel is effectively the uninstall path —
// it deletes every file the app ever wrote (app data, logs, temp WAVs) so
// the user can then simply delete the .exe to be completely clean.
//
// Public surface:
//   delete_app_data_dir()          — wipe %APPDATA%\VeloCut (or platform equivalent)
//   delete_temp_files()            — sweep OS temp dir(s) for velocut_* and velocut.log
//   reset_context()                 — soft in-memory teardown: stop playback,
//                                    drop audio handles, clear caches, wipe egui
//                                    state. Safe to call on the keep-using path.
//   schedule_app_data_dir_deletion — calls reset_context, deletes app data,
//                                    then hard-exits
//   show_uninstall_modal()         — egui overlay shown after reset; "Close" or "Keep Using"
//
// Neither filesystem function panics. All errors are logged via eprintln! and
// treated as non-fatal; a failed delete is always better than a crashed app.

use eframe::egui::{self, Color32, Margin, RichText, Stroke};
use crate::context::AppContext;
use std::collections::HashMap;

// ── Visual constants (local) ──────────────────────────────────────────────────

const DARK_BG_CARD:  Color32 = Color32::from_rgb(22,  24,  32);
const DARK_BG_2:     Color32 = Color32::from_rgb(40,  42,  54);
const DARK_BORDER:   Color32 = Color32::from_rgb(90,  92, 110);
const DARK_TEXT:     Color32 = Color32::WHITE;
const DARK_TEXT_DIM: Color32 = Color32::from_rgb(190, 190, 210);
const GREEN_DIM:     Color32 = Color32::from_rgb(100, 220, 140);
const GREEN_BG:      Color32 = Color32::from_rgb(25,  65,  40);

// ── Filesystem helpers ────────────────────────────────────────────────────────

/// Delete the entire VeloCut application-data directory.
///
/// `eframe::storage_dir("VeloCut")` returns the *data* subdirectory where
/// eframe persists `app.ron` and window geometry, not the top-level app
/// folder. We walk up one level with `.parent()` so the whole tree is
/// removed in one shot, covering sibling directories like `logs/` that
/// would survive a targeted `data/`-only delete.
///
/// Platform paths:
/// ```text
/// Windows : %APPDATA%\VeloCut\data\   <- storage_dir
///           %APPDATA%\VeloCut\         <- deleted here
/// macOS   : ~/Library/Application Support/VeloCut/data/
///           ~/Library/Application Support/VeloCut/
/// Linux   : ~/.local/share/VeloCut/data/
///           ~/.local/share/VeloCut/
/// ```
pub fn delete_app_data_dir() {
    let Some(storage_dir) = eframe::storage_dir("VeloCut") else {
        eprintln!("[reset] eframe::storage_dir returned None - storage not deleted");
        return;
    };

    // .parent() is None only when storage_dir is a filesystem root, which
    // cannot happen for a named app subdirectory. Fall back to storage_dir
    // itself so we at least delete what we can.
    let app_dir = storage_dir
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| storage_dir.clone());

    match std::fs::remove_dir_all(&app_dir) {
        Ok(()) => eprintln!("[reset] deleted VeloCut app data dir '{}'", app_dir.display()),
        Err(e) => eprintln!("[reset] could not delete VeloCut app data dir '{}': {e}", app_dir.display()),
    }
}

/// Delete all VeloCut-owned files from the OS temp directory (or directories).
///
/// Two file patterns are targeted:
/// - `velocut_*`   - audio temp files written by the media worker (e.g. `velocut_<uuid>.wav`)
/// - `velocut.log` - the append-only UI log file written by `vlog()`
///
/// ## Why two directories on Windows (MSYS2 quirk)
///
/// When built with the MSYS2 toolchain, `std::env::temp_dir()` resolves through
/// the MSYS2 POSIX layer to `C:\msys64\tmp\`. Native Windows dependencies (e.g.
/// the ffmpeg bindings) call `GetTempPathW` and receive the real `%TEMP%` path
/// (`C:\Users\<user>\AppData\Local\Temp\`). Files such as `velocut_audio.log`
/// end up in the Windows path, so a single-directory scan misses them.
///
/// On non-Windows platforms the two paths are identical; deduplication ensures
/// we only scan once.
pub fn delete_temp_files() {
    let mut tmp_dirs: Vec<std::path::PathBuf> = vec![std::env::temp_dir()];

    // On Windows we need to sweep the real Windows temp directory, which MSYS2
    // builds may never see via std::env::temp_dir() or %TEMP% — both get
    // rewritten by the MSYS2 runtime to point to C:\msys64\tmp\.
    //
    // %LOCALAPPDATA% is a Windows-native variable that MSYS2 does not override,
    // so %LOCALAPPDATA%\Temp reliably resolves to the real Windows temp dir
    // (C:\Users\<user>\AppData\Local\Temp\) regardless of toolchain.
    //
    // We also still check %TEMP% and %TMP% as best-effort fallbacks in case
    // the binary is ever built outside of MSYS2 and those vars are meaningful.
    // All candidates are deduplicated before scanning.
    #[cfg(target_os = "windows")]
    for var in &["LOCALAPPDATA", "TEMP", "TMP"] {
        let candidate = match var {
            // LOCALAPPDATA points to the AppData\Local folder; Temp is a
            // well-known subdirectory that always exists alongside it.
            &"LOCALAPPDATA" => std::env::var(var).ok()
                .map(|v| std::path::PathBuf::from(v).join("Temp")),
            _ => std::env::var(var).ok()
                .map(std::path::PathBuf::from),
        };
        if let Some(path) = candidate {
            if !tmp_dirs.iter().any(|d| d == &path) {
                tmp_dirs.push(path);
            }
        }
    }

    for dir in tmp_dirs {
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    let is_velocut_file = name_str.starts_with("velocut_")
                        || name_str == "velocut.log";
                    if is_velocut_file {
                        let path = entry.path();
                        match std::fs::remove_file(&path) {
                            Ok(()) => eprintln!("[reset] deleted temp file '{}'", path.display()),
                            Err(e) => eprintln!("[reset] could not delete temp file '{}': {e}", path.display()),
                        }
                    }
                }
            }
            Err(e) => eprintln!("[reset] could not read temp dir '{}': {e}", dir.display()),
        }
    }
}

// ── In-memory teardown ───────────────────────────────────────────────────────

/// Soft in-memory reset — clears all cached state so the app can continue
/// running with a clean slate. Safe to call on the "keep using" path.
///
/// Does NOT call `media_worker.shutdown()` — the worker threads stay alive and
/// ready to probe and decode after the reset. Use this from `ClearProject` in
/// `app.rs` and from the second reset button press in `export_module.rs`.
///
/// For the hard-exit path, `schedule_app_data_dir_deletion()` calls this first
/// and then additionally shuts down the worker before the process exits.
///
/// Teardown order:
///
/// 1. `media_worker.stop_playback()` — signals the pb thread to stop before any
///    state changes. The decode loop holds clip references and races if state
///    changes under it.
/// 2. `audio_sinks.clear()` + `audio_overlay_sinks.clear()` — drops rodio Sinks
///    (which hold WAV file handles) before the OutputStream. Dropping the stream
///    before its sinks panics in rodio.
/// 3. `audio_stream = None` — shuts down the WASAPI/ALSA output stream.
/// 4. `cache.clear_all()` — drops all GPU `TextureHandle` refs, releasing
///    thumbnail, frame, and bucket cache allocations through eframe's backend.
/// 5. `playback.reset()` — clears all scrub / playback decode tracking state.
/// 6. `ctx.memory_mut(|mem| mem.data.clear())` — wipes all egui widget state:
///    scroll positions, collapsible open/close, text cursor positions, popup
///    flags, etc. Leaves the UI blank for the next frame. Pass `None` on the
///    hard-exit path where no live context is available.
pub fn reset_context(context: &mut AppContext, ctx: Option<&egui::Context>) {
    // Stop pb thread before touching any state it references.
    context.media_worker.stop_playback();

    // Drop rodio Sinks before the OutputStream — order is load-bearing.
    // Sinks hold WAV file handles; stream must outlive them.
    context.audio_sinks         = HashMap::new();
    context.audio_overlay_sinks = HashMap::new();
    context.audio_stream = None;

    // Evict all GPU textures and reset the byte budget.
    context.cache.clear_all();

    // Clear scrub / playback decode tracking.
    context.playback.reset();

    // Wipe all egui widget state so the UI starts blank on the next frame.
    // None on the hard-exit path — no live context available, and the process
    // is about to die anyway so the clear would be a no-op regardless.
    if let Some(ctx) = ctx {
        // Flush egui_extras image-loader URI cache. Without this, thumbnail textures
        // loaded via URI strings linger in the loader's internal HashMap and keep
        // occupying GPU memory even after cache.clear_all() drops our TextureHandles.
        ctx.forget_all_images();

        ctx.memory_mut(|mem| {
            // mem.data.clear() only clears the widget TypeMap (panel sizes, scroll
            // positions, collapsible states). It does NOT touch the private fields:
            //   • areas       — positions of every panel/window/Area
            //   • interactions — per-viewport drag and hover tracking
            //   • focus        — which widget holds keyboard focus
            //   • popups       — any open combo-box/context-menu/tooltip
            //   • caches       — frame-to-frame computation caches
            //   • to_global    — per-layer coordinate transforms
            //
            // Replacing the whole Memory with Default resets all of them at once.
            // We preserve `options` so the dark-mode preference isn't wiped.
            let options = mem.options.clone();
            *mem = egui::Memory::default();
            mem.options = options;
        });
    }
}

// ── Deferred app-data deletion ───────────────────────────────────────────────

/// Delete the VeloCut app-data directory and immediately hard-exit the process.
///
/// # Why hard-exit instead of a deferred cmd/ping trick?
///
/// eframe saves egui's own state (window geometry, panel sizes) to the same
/// storage backend as our app data, independently of our `App::save()` override.
/// Even with an early-return guard in `save()`, eframe flushes its internal
/// viewport state and recreates `%APPDATA%\VeloCut\data\` *after* `on_exit()`
/// returns — so any delete that completes before that flush gets immediately
/// undone.
///
/// Calling `std::process::exit(0)` inside `on_exit()` kills the process before
/// eframe's event loop can perform that final write. The directory is gone and
/// nothing recreates it. No external processes, no timers, no race conditions.
///
/// # Resource teardown
///
/// Calls `reset_context()` before deleting the filesystem and exiting.
/// `std::process::exit(0)` skips all Drop impls — `reset_context` is the
/// single place where all handles are explicitly released. See its doc for
/// the full teardown order.
/// 6. `ctx.memory_mut(|mem| mem.data.clear())` — wipes all egui widget state
///    (scroll positions, collapsible open/close, text cursor positions, etc.)
///    so the UI starts blank rather than restoring stale panel layout from the
///    memory that eframe would otherwise persist to the now-deleted app data dir.
///
/// # Usage
///
/// Call this from your `App::on_exit()` implementation when the reset flag is set:
///
/// ```rust
/// fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
///     if self.reset_scheduled {
///         crate::helpers::reset::schedule_app_data_dir_deletion(&mut self.context);
///         // unreachable — process exits inside the call above
///     }
/// }
/// ```
///
/// Also ensure `App::save()` is guarded so eframe does not write during the
/// shutdown sequence before `on_exit()` is reached:
///
/// ```rust
/// fn save(&mut self, _storage: &mut dyn eframe::Storage) {
///     if self.reset_scheduled {
///         return;
///     }
///     // ... normal save logic
/// }
/// ```
pub fn schedule_app_data_dir_deletion(context: &mut AppContext) {
    // Soft teardown first — stops playback, drops audio, clears caches.
    // No egui context available on the exit path, so None is passed — the
    // process is about to die and the egui memory clear would be a no-op.
    // std::process::exit(0) below skips Drop impls, so anything not released
    // here leaks until the OS reclaims the process.
    reset_context(context, None);

    // Shut down the FFmpeg worker threads now. This is intentionally NOT in
    // reset_context — shutdown() permanently joins the worker threads, which
    // would break the app on the "keep using" path. Only safe to call here
    // because we are about to hard-exit anyway.
    // Must happen after reset_context (which calls stop_playback) and before
    // delete_temp_files so no thread still has a WAV temp file open when we
    // try to unlink it.
    context.media_worker.shutdown();

    delete_app_data_dir();

    // Hard-exit now. eframe's post-exit storage flush — which would otherwise
    // recreate the directory we just deleted — never runs because the process
    // is already gone. Exit code 0: this is a clean, user-initiated shutdown.
    std::process::exit(0);
}

// ── Post-reset modal ──────────────────────────────────────────────────────────

/// Full-screen overlay shown after a confirmed reset.
///
/// Informs the user that all app data has been erased and that they can delete
/// `VeloCut.exe` to finish uninstalling. Offers two exits:
///
/// - **Close VeloCut** - sends `ViewportCommand::Close`, exits immediately.
/// - **Keep Using**    - dismisses the overlay; the now-blank app stays open.
///
/// `visible` is the `show_reset_complete` flag on `ExportModule`. Set it to
/// `true` at the confirmed-click site; this function sets it back to `false`
/// when either button is pressed.
///
/// Call from `render_panels` after all other overlays so it paints on top:
/// ```rust
/// crate::helpers::reset::show_uninstall_modal(ctx, &mut self.export.show_reset_complete);
/// ```
pub fn show_uninstall_modal(ctx: &egui::Context, visible: &mut bool) {
    if !*visible {
        return;
    }

    let screen = ctx.viewport_rect();

    // ── Scrim ─────────────────────────────────────────────────────────────────
    // Heavier opacity than the render modal (180 vs 128) — this is a terminal
    // state; we want the background app to feel clearly inert behind the card.
    let scrim_painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("reset_modal_scrim"),
    ));
    scrim_painter.rect_filled(screen, 0.0, Color32::from_black_alpha(180));

    // ── Card geometry ─────────────────────────────────────────────────────────
    const CARD_W: f32 = 420.0;
    const CARD_H: f32 = 310.0;
    const PAD:    f32 = 28.0;

    let card_rect  = egui::Rect::from_center_size(screen.center(), egui::vec2(CARD_W, CARD_H));
    let inner_rect = card_rect.shrink(PAD);

    egui::Area::new(egui::Id::new("reset_modal_card"))
        .order(egui::Order::Tooltip)
        .fixed_pos(card_rect.min)
        .show(ctx, |ui| {
            ui.set_min_size(card_rect.size());
            ui.set_max_size(card_rect.size());

            // Solid card background — fully opaque so the scrim does not bleed
            // through and dim text inside the card.
            ui.painter().rect(
                card_rect,
                egui::CornerRadius::same(6),
                DARK_BG_CARD,
                Stroke::new(1.5, GREEN_DIM),
                egui::StrokeKind::Inside,
            );

            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner_rect));
            show_modal_content(&mut child, visible);
        });
}

fn show_modal_content(ui: &mut egui::Ui, visible: &mut bool) {
    // ── Title ─────────────────────────────────────────────────────────────────
    ui.label(
        RichText::new("  Uninstall Complete")
            .size(15.0)
            .strong()
            .color(GREEN_DIM),
    );

    ui.add_space(14.0);

    // ── Deleted items summary ─────────────────────────────────────────────────
    egui::Frame::new()
        .fill(GREEN_BG)
        .stroke(Stroke::new(1.0, GREEN_DIM))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for line in &[
                "  Project clips and library cleared",
                "  App settings and window layout deleted",
                "  Temporary audio files deleted",
                "  Log files deleted",
            ] {
                ui.label(RichText::new(*line).size(11.0).color(GREEN_DIM));
            }
        });

    ui.add_space(14.0);

    // ── Uninstall prompt ──────────────────────────────────────────────────────
    ui.label(
        RichText::new("VeloCut.exe is the only remaining file.")
            .size(12.0)
            .strong()
            .color(DARK_TEXT),
    );
    ui.add_space(4.0);
    ui.label(
        RichText::new("Delete it from wherever you saved it to finish uninstalling.")
            .size(11.0)
            .color(DARK_TEXT_DIM),
    );

    ui.add_space(18.0);

    // ── Buttons ───────────────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        let btn_w = (ui.available_width() - 8.0) / 2.0;

        // Primary: close the process entirely
        let close_btn = egui::Button::new(
            RichText::new("Close VeloCut").size(12.0).strong().color(Color32::BLACK),
        )
        .fill(GREEN_DIM)
        .stroke(Stroke::NONE)
        .min_size(egui::vec2(btn_w, 32.0));

        if ui.add(close_btn).clicked() {
            *visible = false;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        ui.add_space(8.0);

        // Secondary: keep the blank app running
        let keep_btn = egui::Button::new(
            RichText::new("Keep Using").size(12.0).color(DARK_TEXT_DIM),
        )
        .fill(DARK_BG_2)
        .stroke(Stroke::new(1.0, DARK_BORDER))
        .min_size(egui::vec2(btn_w, 32.0));

        if ui.add(keep_btn).clicked() {
            *visible = false;
        }
    });
}