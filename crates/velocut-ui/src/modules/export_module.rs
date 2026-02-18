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
//
// Resolution model:
//   Resolutions are expressed as a *quality level* (short-side pixel count)
//   rather than fixed pixel dimensions. The actual width × height are derived
//   from the quality level and the export aspect ratio at render time, so a
//   "720p" export at 9:16 produces 720×1280 while the same quality level at
//   16:9 produces 1280×720. Both dimensions are rounded to the nearest even
//   number (required for YUV420P).
//
//   The export aspect ratio defaults to the project's current aspect ratio
//   (read from ProjectState), but can be overridden per-export using the
//   "Aspect Ratio" ComboBox in the settings UI.

use super::EditorModule;
use velocut_core::state::{ProjectState, AspectRatio};
use velocut_core::commands::EditorCommand;
use crate::modules::ThumbnailCache;
use crate::theme::{ACCENT, DARK_BG_2, DARK_BG_3, DARK_BORDER, DARK_TEXT_DIM};
use egui::{Color32, Margin, RichText, Stroke, Ui};

// ── Colour palette extensions (local to this module) ─────────────────────────

/// Muted green used for the "done" success banner.
const GREEN_DIM: Color32 = Color32::from_rgb(80,  190, 120);
/// Muted red used for error / cancel banners.
const RED_DIM:   Color32 = Color32::from_rgb(200, 80,  80);
/// Background fill for the progress bar track.
const TRACK_BG:  Color32 = Color32::from_rgb(35,  35,  40);
/// Filled portion of the progress bar.
const TRACK_FG:  Color32 = Color32::from_rgb(90,  160, 255);

// ── Quality preset ────────────────────────────────────────────────────────────

/// Output quality expressed as the short-side pixel count.
///
/// Width and height are derived from this + the export aspect ratio at render
/// time, so the same quality level produces different pixel counts for
/// landscape vs. portrait vs. square outputs.
#[derive(PartialEq, Clone, Copy)]
enum QualityPreset {
    SD480,
    HD720,
    FHD1080,
    QHD1440,
    UHD4K,
}

impl QualityPreset {
    fn label(self) -> &'static str {
        match self {
            QualityPreset::SD480   => "480p",
            QualityPreset::HD720   => "720p  (HD)",
            QualityPreset::FHD1080 => "1080p (Full HD)",
            QualityPreset::QHD1440 => "1440p (2K)",
            QualityPreset::UHD4K   => "4K    (2160p)",
        }
    }

    /// Pixel count of the short side.
    fn short_side(self) -> u32 {
        match self {
            QualityPreset::SD480   => 480,
            QualityPreset::HD720   => 720,
            QualityPreset::FHD1080 => 1080,
            QualityPreset::QHD1440 => 1440,
            QualityPreset::UHD4K   => 2160,
        }
    }

    /// Compute (width, height) for a given aspect ratio.
    ///
    /// The short side is always `self.short_side()` pixels. The long side is
    /// derived from the ratio. Both values are rounded to the nearest even
    /// number (required for H.264 YUV420P encoding).
    fn dimensions(self, ratio: f32) -> (u32, u32) {
        let s = self.short_side() as f32;
        let (w, h) = if ratio >= 1.0 {
            // Landscape or square: height is the short side.
            let w = (s * ratio).round() as u32;
            let h = s as u32;
            (w, h)
        } else {
            // Portrait: width is the short side.
            let w = s as u32;
            let h = (s / ratio).round() as u32;
            (w, h)
        };
        // Round each dimension up to the nearest even number.
        ((w + 1) & !1, (h + 1) & !1)
    }
}

// ── Aspect ratio helpers ──────────────────────────────────────────────────────

fn aspect_ratio_value(ar: AspectRatio) -> f32 {
    match ar {
        AspectRatio::SixteenNine   => 16.0 / 9.0,
        AspectRatio::NineSixteen   => 9.0  / 16.0,
        AspectRatio::TwoThree      => 2.0  / 3.0,
        AspectRatio::ThreeTwo      => 3.0  / 2.0,
        AspectRatio::FourThree     => 4.0  / 3.0,
        AspectRatio::OneOne        => 1.0,
        AspectRatio::FourFive      => 4.0  / 5.0,
        AspectRatio::TwentyOneNine => 21.0 / 9.0,
        AspectRatio::Anamorphic    => 2.39,
    }
}

fn aspect_ratio_label(ar: AspectRatio) -> &'static str {
    match ar {
        AspectRatio::SixteenNine   => "16:9  — Landscape / YouTube",
        AspectRatio::NineSixteen   => "9:16  — Portrait / Reels / Shorts",
        AspectRatio::FourThree     => "4:3   — Classic TV",
        AspectRatio::ThreeTwo      => "3:2   — Landscape photo",
        AspectRatio::TwoThree      => "2:3   — Portrait photo",
        AspectRatio::OneOne        => "1:1   — Square",
        AspectRatio::FourFive      => "4:5   — Instagram portrait",
        AspectRatio::TwentyOneNine => "21:9  — Ultrawide / Cinema",
        AspectRatio::Anamorphic    => "2.39  — Anamorphic widescreen",
    }
}

const ALL_ASPECT_RATIOS: &[AspectRatio] = &[
    AspectRatio::SixteenNine,
    AspectRatio::NineSixteen,
    AspectRatio::OneOne,
    AspectRatio::FourFive,
    AspectRatio::FourThree,
    AspectRatio::ThreeTwo,
    AspectRatio::TwoThree,
    AspectRatio::TwentyOneNine,
    AspectRatio::Anamorphic,
];

// ── Module ────────────────────────────────────────────────────────────────────

pub struct ExportModule {
    filename: String,
    quality:  QualityPreset,
    fps:      u32,
    /// Export aspect ratio override. `None` = follow the project's aspect ratio.
    /// Stored as `Option` so we can show a "Match Project" default and switch
    /// back to automatic if the project ratio changes.
    export_aspect: Option<AspectRatio>,
}

impl Default for ExportModule {
    fn default() -> Self {
        Self {
            filename:      "sequence_01".into(),
            quality:       QualityPreset::FHD1080,
            fps:           30,
            export_aspect: None, // follows project
        }
    }
}

impl EditorModule for ExportModule {
    fn name(&self) -> &str { "Export" }

    fn ui(
        &mut self,
        ui:           &mut Ui,
        state:        &ProjectState,
        _thumb_cache: &mut ThumbnailCache,
        cmd:          &mut Vec<EditorCommand>,
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
                // to match the dark theme without egui's default blue.
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

    /// Filename / aspect ratio / quality / fps / stats / render button.
    fn show_settings_ui(
        &mut self,
        ui:          &mut Ui,
        state:       &ProjectState,
        cmd:         &mut Vec<EditorCommand>,
        is_encoding: bool,
    ) {
        // Resolve the effective aspect ratio and its f32 value for dimension math.
        let effective_ar    = self.export_aspect.unwrap_or(state.aspect_ratio);
        let effective_ratio = aspect_ratio_value(effective_ar);

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

        // ── Aspect Ratio ──────────────────────────────────────────────────────
        // Defaults to the project ratio; user can override per-export without
        // changing the project-level setting.
        ui.label(RichText::new("Aspect Ratio").size(11.0).color(DARK_TEXT_DIM));
        ui.add_space(2.0);
        ui.add_enabled_ui(!is_encoding, |ui| {
            // Label shown in the collapsed combo.
            let combo_label = if self.export_aspect.is_none() {
                format!("↩ Match Project  ({})", aspect_ratio_label(state.aspect_ratio))
            } else {
                aspect_ratio_label(effective_ar).to_string()
            };

            egui::ComboBox::from_id_salt("export_aspect_ratio")
                .selected_text(&combo_label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    // "Match Project" option always at the top.
                    let match_selected = self.export_aspect.is_none();
                    let match_label = format!(
                        "↩ Match Project  ({})",
                        aspect_ratio_label(state.aspect_ratio)
                    );
                    if ui.selectable_label(match_selected, &match_label).clicked() {
                        self.export_aspect = None;
                    }

                    ui.separator();

                    // One entry per aspect ratio variant.
                    for &ar in ALL_ASPECT_RATIOS {
                        let selected = self.export_aspect == Some(ar);
                        if ui.selectable_label(selected, aspect_ratio_label(ar)).clicked() {
                            self.export_aspect = Some(ar);
                        }
                    }
                });
        });

        ui.add_space(10.0);

        // ── Quality / Resolution ──────────────────────────────────────────────
        // Shown as quality levels (short-side px). Actual pixel dimensions are
        // displayed below the ComboBox so the user can see the final resolution.
        ui.label(RichText::new("Quality").size(11.0).color(DARK_TEXT_DIM));
        ui.add_space(2.0);
        ui.add_enabled_ui(!is_encoding, |ui| {
            egui::ComboBox::from_id_salt("quality_preset")
                .selected_text(self.quality.label())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for q in [
                        QualityPreset::SD480,
                        QualityPreset::HD720,
                        QualityPreset::FHD1080,
                        QualityPreset::QHD1440,
                        QualityPreset::UHD4K,
                    ] {
                        let (w, h) = q.dimensions(effective_ratio);
                        let label  = format!("{}  — {w}×{h}", q.label());
                        ui.selectable_value(&mut self.quality, q, label);
                    }
                });
        });

        // Show the resolved pixel dimensions below the ComboBox as a hint.
        let (res_w, res_h) = self.quality.dimensions(effective_ratio);
        ui.add_space(2.0);
        ui.label(
            RichText::new(format!("{res_w} × {res_h} px"))
                .size(10.0)
                .color(DARK_TEXT_DIM),
        );

        ui.add_space(10.0);

        // ── Frame Rate ────────────────────────────────────────────────────────
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

                let total      = state.timeline.iter()
                    .map(|c| c.start_time + c.duration)
                    .fold(0.0_f64, f64::max);
                let clips      = state.timeline.len();
                let est_frames = (total * self.fps as f64).ceil() as u64;
                let has_audio  = state.library.iter().any(|lc| {
                    state.timeline.iter().any(|tc| tc.media_id == lc.id)
                        && lc.audio_path.is_some()
                });

                ui.label(RichText::new(format!("Duration:  {total:.1}s")).size(11.0).monospace());
                ui.label(RichText::new(format!("Clips:     {clips}")).size(11.0).monospace());
                ui.label(RichText::new(format!("Output:    {res_w}×{res_h} @ {}fps", self.fps)).size(11.0).monospace());
                ui.label(RichText::new(format!("Frames:    ~{est_frames}")).size(11.0).monospace());
                ui.label(RichText::new(format!(
                    "Audio:     {}",
                    if has_audio { "AAC 128kbps stereo" } else { "none detected" }
                )).size(11.0).monospace());
                ui.label(RichText::new("Video:     H.264 CRF 18").size(11.0).monospace());
            });

        ui.add_space(12.0);

        // ── Render button (hidden while encoding; replaced by Cancel) ─────────
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
                cmd.push(EditorCommand::RenderMP4 {
                    filename: self.filename.clone(),
                    width:    res_w,
                    height:   res_h,
                    fps:      self.fps,
                });
            }
            if no_clips {
                response.on_hover_text("Add clips to the timeline first");
            }
        }
    }
}