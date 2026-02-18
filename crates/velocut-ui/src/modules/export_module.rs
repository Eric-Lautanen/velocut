// crates/velocut-ui/src/modules/export_module.rs
//
// ExportModule: right-panel UI for configuring and launching an MP4 render.
//
// State machine (driven by ProjectState encode fields, set by AppContext):
//
//   Idle       → user clicks "Render MP4"
//                → app.rs opens rfd save dialog, calls media_worker.start_encode
//                → state.encode_job = Some(job_id)
//
//   Encoding   → EncodeProgress results arrive each PROGRESS_INTERVAL frames
//                → state.encode_progress = Some((frame, total))
//                → UI shows progress bar + Cancel button
//
//   Done       → state.encode_done = Some(path)  (set by ingest_media_results)
//                → UI shows ✓ banner, clears job after user acknowledges
//
//   Error      → state.encode_error = Some(msg)
//                → UI shows ✗ banner (includes "cancelled" from user cancel)

use super::EditorModule;
use velocut_core::state::ProjectState;
use velocut_core::commands::EditorCommand;
use crate::modules::ThumbnailCache;
use crate::theme::{ACCENT, DARK_BG_2, DARK_BG_3, DARK_BORDER, DARK_TEXT_DIM};
use egui::{Color32, Margin, RichText, Stroke, Ui};

// ── Colour palette extensions (local to this module) ─────────────────────────

/// Muted green used for the "done" success banner.
const GREEN_DIM: Color32  = Color32::from_rgb(80,  190, 120);
/// Muted red used for error / cancel banners.
const RED_DIM: Color32    = Color32::from_rgb(200, 80,  80);
/// Background fill for the progress bar track.
const TRACK_BG: Color32   = Color32::from_rgb(35,  35,  40);
/// Filled portion of the progress bar.
const TRACK_FG: Color32   = Color32::from_rgb(90,  160, 255);

// ── Module ────────────────────────────────────────────────────────────────────

pub struct ExportModule {
    filename:   String,
    resolution: ResolutionPreset,
    fps:        u32,
}

#[derive(PartialEq, Clone, Copy)]
enum ResolutionPreset { HD, FHD, UHD }

impl Default for ExportModule {
    fn default() -> Self {
        Self {
            filename:   "sequence_01".into(),
            resolution: ResolutionPreset::FHD,
            fps:        30,
        }
    }
}

impl EditorModule for ExportModule {
    fn name(&self) -> &str { "Export" }

    fn ui(
        &mut self,
        ui:          &mut Ui,
        state:       &ProjectState,
        _thumb_cache: &mut ThumbnailCache,
        cmd:         &mut Vec<EditorCommand>,
    ) {
        ui.vertical(|ui| {
            // ── Header ────────────────────────────────────────────────────────
            egui::Frame::new()
                .fill(DARK_BG_2)
                .inner_margin(Margin { left: 8, right: 8, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.label(RichText::new("🚀 Export").size(12.0).strong());
                });

            ui.separator();
            ui.add_space(6.0);

            // ── Encode-in-progress overlay ────────────────────────────────────
            // Shown while a job is running. The rest of the settings UI is still
            // visible but the render button is replaced by a Cancel button so the
            // user can abort without needing to find another control.
            let is_encoding = state.encode_job.is_some()
                && state.encode_done.is_none()
                && state.encode_error.is_none();

            if is_encoding {
                self.show_progress_ui(ui, state, cmd);
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
            }

            // ── Done banner ───────────────────────────────────────────────────
            if let Some(path) = &state.encode_done {
                let label = path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                egui::Frame::new()
                    .fill(Color32::from_rgb(30, 60, 40))
                    .stroke(Stroke::new(1.0, GREEN_DIM))
                    .corner_radius(egui::CornerRadius::same(4))
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(format!("✓ Saved: {label}"))
                                .size(11.0)
                                .color(GREEN_DIM),
                        );
                        if ui.small_button("Dismiss").clicked() {
                            cmd.push(EditorCommand::ClearEncodeStatus);
                        }
                    });
                ui.add_space(8.0);
            }

            // ── Error / cancelled banner ──────────────────────────────────────
            if let Some(msg) = &state.encode_error {
                let display = if msg == "cancelled" {
                    "✕ Render cancelled".to_string()
                } else {
                    format!("✕ Error: {msg}")
                };
                egui::Frame::new()
                    .fill(Color32::from_rgb(60, 25, 25))
                    .stroke(Stroke::new(1.0, RED_DIM))
                    .corner_radius(egui::CornerRadius::same(4))
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(&display).size(11.0).color(RED_DIM));
                        if ui.small_button("Dismiss").clicked() {
                            cmd.push(EditorCommand::ClearEncodeStatus);
                        }
                    });
                ui.add_space(8.0);
            }

            // ── Settings (always visible) ─────────────────────────────────────
            ui.vertical(|ui| {
                ui.add_space(4.0);
                self.show_settings_ui(ui, state, cmd, is_encoding);
            });
        });
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

impl ExportModule {
    /// Progress bar + cancel button shown while encoding.
    fn show_progress_ui(
        &self,
        ui:    &mut Ui,
        state: &ProjectState,
        cmd:   &mut Vec<EditorCommand>,
    ) {
        egui::Frame::new()
            .fill(DARK_BG_3)
            .stroke(Stroke::new(1.0, DARK_BORDER))
            .corner_radius(egui::CornerRadius::same(4))
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());

                let (frame, total) = state.encode_progress.unwrap_or((0, 1));
                let fraction       = (frame as f32 / total as f32).clamp(0.0, 1.0);
                let pct            = (fraction * 100.0) as u32;

                ui.label(
                    RichText::new(format!("Encoding… {pct}% ({frame} / {total} frames)"))
                        .size(11.0)
                        .color(DARK_TEXT_DIM),
                );
                ui.add_space(6.0);

                // Draw the progress bar with raw painter calls so we can style it
                // to match the rest of the dark theme without egui's default blue.
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 8.0),
                    egui::Sense::hover(),
                );
                let painter = ui.painter();
                painter.rect_filled(rect, 4.0, TRACK_BG);
                if fraction > 0.0 {
                    let mut filled = rect;
                    filled.max.x = rect.min.x + rect.width() * fraction;
                    painter.rect_filled(filled, 4.0, TRACK_FG);
                }

                ui.add_space(8.0);

                // Cancel button — full width, danger-coloured.
                let cancel_btn = egui::Button::new(
                    RichText::new("✕ Cancel").size(11.0).color(RED_DIM),
                )
                .stroke(Stroke::new(1.0, RED_DIM))
                .fill(Color32::from_rgb(50, 20, 20))
                .min_size(egui::vec2(ui.available_width(), 26.0));

                if ui.add(cancel_btn).clicked() {
                    if let Some(job_id) = state.encode_job {
                        cmd.push(EditorCommand::CancelEncode(job_id));
                    }
                }
            });
    }

    /// Filename / resolution / fps / stats / render button.
    fn show_settings_ui(
        &mut self,
        ui:          &mut Ui,
        state:       &ProjectState,
        cmd:         &mut Vec<EditorCommand>,
        is_encoding: bool,
    ) {
        ui.add_space(4.0);

        // ── Filename ──────────────────────────────────────────────────────────
        ui.label(RichText::new("Output Name").size(11.0).color(DARK_TEXT_DIM));
        ui.add_space(2.0);
        ui.add_enabled(
            !is_encoding,
            egui::TextEdit::singleline(&mut self.filename)
                .desired_width(f32::INFINITY)
                .hint_text("filename…"),
        );

        ui.add_space(10.0);

        // ── Resolution ────────────────────────────────────────────────────────
        ui.label(RichText::new("Resolution").size(11.0).color(DARK_TEXT_DIM));
        ui.add_space(2.0);
        ui.add_enabled_ui(!is_encoding, |ui| {
            egui::ComboBox::from_id_salt("resolution_preset")
                .selected_text(match self.resolution {
                    ResolutionPreset::HD  => "1280 × 720  (HD)",
                    ResolutionPreset::FHD => "1920 × 1080 (FHD)",
                    ResolutionPreset::UHD => "3840 × 2160 (4K)",
                })
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.resolution, ResolutionPreset::HD,  "1280 × 720  (HD)");
                    ui.selectable_value(&mut self.resolution, ResolutionPreset::FHD, "1920 × 1080 (FHD)");
                    ui.selectable_value(&mut self.resolution, ResolutionPreset::UHD, "3840 × 2160 (4K)");
                });
        });

        ui.add_space(10.0);

        // ── Frame rate ────────────────────────────────────────────────────────
        ui.label(RichText::new("Frame Rate").size(11.0).color(DARK_TEXT_DIM));
        ui.add_space(2.0);
        ui.add_enabled_ui(!is_encoding, |ui| {
            ui.horizontal(|ui| {
                for &rate in &[24u32, 30, 60] {
                    let selected = self.fps == rate;
                    let btn = egui::Button::new(
                        RichText::new(format!("{rate} fps"))
                            .size(11.0)
                            .color(if selected { ACCENT } else { DARK_TEXT_DIM }),
                    )
                    .stroke(Stroke::new(1.0, if selected { ACCENT } else { DARK_BORDER }))
                    .fill(if selected { DARK_BG_3 } else { DARK_BG_2 });

                    if ui.add(btn).clicked() {
                        self.fps = rate;
                    }
                }
            });
        });

        ui.add_space(10.0);

        // ── Stats ─────────────────────────────────────────────────────────────
        egui::Frame::new()
            .fill(DARK_BG_3)
            .stroke(Stroke::new(1.0, DARK_BORDER))
            .corner_radius(egui::CornerRadius::same(4))
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let total = state.timeline.iter()
                    .map(|c| c.start_time + c.duration)
                    .fold(0.0_f64, f64::max);
                let clips = state.timeline.len();
                let (w, h) = match self.resolution {
                    ResolutionPreset::HD  => (1280u32, 720u32),
                    ResolutionPreset::FHD => (1920,    1080),
                    ResolutionPreset::UHD => (3840,    2160),
                };
                let est_frames = (total * self.fps as f64).ceil() as u64;
                ui.label(RichText::new(format!("Duration:  {total:.1}s")).size(11.0).monospace());
                ui.label(RichText::new(format!("Clips:     {clips}")).size(11.0).monospace());
                ui.label(RichText::new(format!("Output:    {w}×{h} @ {}fps", self.fps)).size(11.0).monospace());
                ui.label(RichText::new(format!("Frames:    ~{est_frames}")).size(11.0).monospace());
                ui.label(RichText::new("Format:    MP4 / H.264 CRF 18").size(11.0).monospace());
            });

        ui.add_space(12.0);

        // ── Render button (hidden while encoding; progress UI has Cancel instead) ──
        if !is_encoding {
            let no_clips = state.timeline.is_empty();
            let render_btn = egui::Button::new(
                RichText::new("⚡ Render MP4")
                    .size(13.0)
                    .strong()
                    .color(if no_clips { Color32::DARK_GRAY } else { Color32::BLACK }),
            )
            .fill(if no_clips { DARK_BG_3 } else { ACCENT })
            .stroke(Stroke::NONE)
            .min_size(egui::vec2(ui.available_width(), 34.0));

            let response = ui.add_enabled(!no_clips, render_btn);
            if response.clicked() {
                let (width, height) = match self.resolution {
                    ResolutionPreset::HD  => (1280u32, 720u32),
                    ResolutionPreset::FHD => (1920,    1080),
                    ResolutionPreset::UHD => (3840,    2160),
                };
                cmd.push(EditorCommand::RenderMP4 {
                    filename: self.filename.clone(),
                    width,
                    height,
                    fps: self.fps,
                });
            }
            if no_clips {
                response.on_hover_text("Add clips to the timeline first");
            }
        }
    }
}