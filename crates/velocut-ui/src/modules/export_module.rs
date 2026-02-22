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
use velocut_core::helpers::geometry::{aspect_ratio_value, aspect_ratio_label};
use crate::modules::ThumbnailCache;
use crate::theme::{ACCENT, DARK_BG_2, DARK_BG_3, DARK_BORDER, DARK_TEXT_DIM};
use egui::{Color32, Context, Margin, RichText, Stroke, Ui};

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

// ── Aspect ratio constants ────────────────────────────────────────────────────

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
    /// Timestamp of when the first "Reset" click happened.
    /// `None` = normal state.  `Some(t)` = waiting for confirmation within 5 s.
    /// Auto-expires — checked and cleared on every render frame.
    clear_confirm_at: Option<std::time::Instant>,
}

impl Default for ExportModule {
    fn default() -> Self {
        Self {
            filename:         "sequence_01".into(),
            quality:          QualityPreset::FHD1080,
            fps:              30,
            export_aspect:    None, // follows project
            clear_confirm_at: None,
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
            // Compute encode state early — needed by both the header reset button
            // (to disable it while rendering) and the progress overlay below.
            let is_encoding = state.encode_job.is_some()
                && state.encode_done.is_none()
                && state.encode_error.is_none();

            // ── Header ────────────────────────────────────────────────────────
            egui::Frame::new()
                .fill(DARK_BG_2)
                .inner_margin(Margin { left: 8, right: 8, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("🚀 Export").size(12.0).strong());

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // ── Two-stage "Reset Everything" button ───────────
                            // Grok-style confirm: first click arms the timer;
                            // second click within 5 s fires ClearProject; after
                            // 5 s the button resets with no action taken.
                            // Disabled while an encode is running.

                            // Cancel any pending confirm if an encode starts so
                            // the user can't accidentally wipe mid-render.
                            if is_encoding {
                                self.clear_confirm_at = None;
                            }
                            // Auto-expire the confirmation window.
                            if let Some(started) = self.clear_confirm_at {
                                if started.elapsed().as_secs_f32() >= 5.0 {
                                    self.clear_confirm_at = None;
                                }
                            }

                            let in_confirm = self.clear_confirm_at.is_some();

                            let btn_label: String = if in_confirm {
                                let secs_left = (5.0_f32
                                    - self.clear_confirm_at.unwrap()
                                        .elapsed().as_secs_f32())
                                    .ceil() as u32;
                                // Drive the countdown without relying on input events.
                                ui.ctx().request_repaint_after(
                                    std::time::Duration::from_millis(250),
                                );
                                format!("⚠ {}s?", secs_left)
                            } else {
                                "🔄 Reset".into()
                            };

                            let (text_color, fill, border) = if in_confirm {
                                (
                                    Color32::from_rgb(255, 160, 50),
                                    Color32::from_rgb(55, 38, 10),
                                    Color32::from_rgb(180, 110, 25),
                                )
                            } else {
                                (DARK_TEXT_DIM, DARK_BG_3, DARK_BORDER)
                            };

                            let reset_btn = egui::Button::new(
                                RichText::new(&btn_label).size(10.0).color(text_color),
                            )
                            .fill(fill)
                            .stroke(Stroke::new(1.0, border))
                            .min_size(egui::vec2(62.0, 20.0));

                            let hover_tip = if in_confirm {
                                "Click again to erase all clips, library, and temp files — cannot be undone"
                            } else if is_encoding {
                                "Stop the render before resetting"
                            } else {
                                "Reset: clear all clips, library entries, and temp files"
                            };

                            if ui.add_enabled(!is_encoding, reset_btn)
                                .on_hover_text(hover_tip)
                                .clicked()
                            {
                                if in_confirm {
                                    cmd.push(EditorCommand::ClearProject);
                                    self.clear_confirm_at = None;
                                } else {
                                    self.clear_confirm_at = Some(std::time::Instant::now());
                                }
                            }
                        });
                    });
                });

            ui.separator();

            // Wrap settings in a scroll area so controls are reachable when
            // the panel is shorter than its natural content height. The bar
            // only appears when the content actually overflows — no bar at
            // normal panel heights.
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.add_space(4.0);
                        self.show_settings_ui(ui, state, cmd, is_encoding);
                    });
                });
        });
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

impl ExportModule {
    /// Full-screen modal overlay for all render status (encoding / done / error).
    ///
    /// Call this from app.rs::update() *after* all panels so it paints on top.
    /// No-op when encode_job is None. Fixed card size — no layout jumping.
    ///
    /// Layer order (bottom → top):
    ///   panels  →  scrim (Foreground painter, drawn first)
    ///           →  card  (Area::Foreground, same order, drawn after — wins)
    pub fn show_render_modal(
        &self,
        ctx:   &Context,
        state: &ProjectState,
        cmd:   &mut Vec<EditorCommand>,
    ) {
        if state.encode_job.is_none() {
            return;
        }

        let screen = ctx.screen_rect();

        // ── Scrim ─────────────────────────────────────────────────────────────
        // 0.5 opacity black over the entire window, painted on the Foreground
        // layer before the card Area so the card renders on top.
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("render_modal_scrim"),
        ));
        painter.rect_filled(screen, 0.0, Color32::from_black_alpha(128));

        // ── Card geometry — fixed, never changes ──────────────────────────────
        const CARD_W: f32 = 440.0;
        const CARD_H: f32 = 270.0;
        const PAD:    f32 = 28.0;

        let card_rect = egui::Rect::from_center_size(
            screen.center(),
            egui::vec2(CARD_W, CARD_H),
        );
        let inner_rect = card_rect.shrink(PAD);

        // Decide border colour from current state.
        let is_done  = state.encode_done.is_some();
        let is_error = state.encode_error.is_some();
        let border_col = if is_done {
            GREEN_DIM
        } else if is_error {
            RED_DIM
        } else {
            TRACK_FG
        };

        // ── Card content Area ─────────────────────────────────────────────────
        // The card background is painted first *inside* the Area so it is
        // guaranteed to be in the same layer as the widgets and behind them.
        // A separate painter layer for the background risks compositing on top.
        egui::Area::new(egui::Id::new("render_modal_content"))
            .order(egui::Order::Foreground)
            .fixed_pos(card_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(card_rect.size());
                ui.set_max_size(card_rect.size());

                // Paint card background first — same layer as widgets so it's
                // always behind them. 0.7 opacity dark fill, sharp edges.
                ui.painter().rect(
                    card_rect,
                    0.0,
                    Color32::from_rgba_unmultiplied(10, 10, 16, 179),
                    Stroke::new(1.0, border_col),
                    egui::StrokeKind::Inside,
                );

                // Inset content by PAD so widgets sit inside the card border.
                let mut child = ui.new_child(
                    egui::UiBuilder::new().max_rect(inner_rect),
                );

                if is_done {
                    self.modal_done(&mut child, state, cmd);
                } else if is_error {
                    self.modal_error(&mut child, state, cmd);
                } else {
                    self.modal_encoding(&mut child, state, cmd);
                    ctx.request_repaint();
                }
            });

        // ── Click-outside-to-close (done / error only) ────────────────────────
        if is_done || is_error {
            let clicked_outside = ctx.input(|i| {
                i.pointer.any_click() && i.pointer.interact_pos()
                    .map(|p| !card_rect.contains(p))
                    .unwrap_or(false)
            });
            if clicked_outside {
                cmd.push(EditorCommand::ClearEncodeStatus);
            }
        }
    }

    // ── Modal state content ───────────────────────────────────────────────────

    fn modal_encoding(&self, ui: &mut Ui, state: &ProjectState, cmd: &mut Vec<EditorCommand>) {
        let (frame, total) = state.encode_progress.unwrap_or((0, 1));
        let fraction = (frame as f32 / total as f32).clamp(0.0, 1.0);
        let pct      = (fraction * 100.0) as u32;

        // Title
        ui.label(RichText::new("Rendering…").size(13.0).strong().color(Color32::WHITE));
        ui.add_space(14.0);

        // Percentage readout
        ui.label(
            RichText::new(format!("{pct}%"))
                .size(46.0)
                .strong()
                .color(TRACK_FG),
        );
        ui.add_space(10.0);

        // Progress bar — same raw-painter approach as the original
        let (bar_rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 8.0),
            egui::Sense::hover(),
        );
        let p = ui.painter();
        p.rect_filled(bar_rect, 4.0, TRACK_BG);
        if fraction > 0.0 {
            let mut fill = bar_rect;
            fill.max.x = bar_rect.min.x + bar_rect.width() * fraction;
            p.rect_filled(fill, 4.0, TRACK_FG);
        }
        ui.add_space(6.0);

        // Frame counter
        ui.label(
            RichText::new(format!("frame {frame}  /  {total}"))
                .size(10.0)
                .color(DARK_TEXT_DIM),
        );
        ui.add_space(14.0);

        // Cancel — full width, neutral (same as original)
        let cancel_btn = egui::Button::new(
            RichText::new("✋  Stop Render").size(11.0).color(DARK_TEXT_DIM),
        )
        .stroke(Stroke::new(1.0, DARK_BORDER))
        .fill(DARK_BG_2)
        .min_size(egui::vec2(ui.available_width(), 28.0));

        if ui.add(cancel_btn).clicked() {
            if let Some(job_id) = state.encode_job {
                cmd.push(EditorCommand::CancelEncode(job_id));
            }
        }
    }

    fn modal_done(&self, ui: &mut Ui, state: &ProjectState, cmd: &mut Vec<EditorCommand>) {
        let label = state.encode_done.as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        ui.label(RichText::new("Export complete").size(13.0).strong().color(Color32::WHITE));
        ui.add_space(14.0);

        // Success frame — same colours as original done banner
        egui::Frame::new()
            .fill(Color32::from_rgb(30, 60, 40))
            .stroke(Stroke::new(1.0, GREEN_DIM))
            .corner_radius(egui::CornerRadius::same(4))
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(format!("🎉  Saved: {label}"))
                        .size(11.0)
                        .color(GREEN_DIM),
                );
            });

        ui.add_space(14.0);

        let dismiss = egui::Button::new(
            RichText::new("Dismiss").size(11.0).color(DARK_TEXT_DIM),
        )
        .stroke(Stroke::new(1.0, DARK_BORDER))
        .fill(DARK_BG_2)
        .min_size(egui::vec2(ui.available_width(), 28.0));

        if ui.add(dismiss).clicked() {
            cmd.push(EditorCommand::ClearEncodeStatus);
        }
    }

    fn modal_error(&self, ui: &mut Ui, state: &ProjectState, cmd: &mut Vec<EditorCommand>) {
        let msg = state.encode_error.as_deref().unwrap_or("");
        let display = if msg == "cancelled" {
            "💥  Render cancelled".to_string()
        } else {
            format!("💥  Error: {msg}")
        };

        ui.label(RichText::new("Render stopped").size(13.0).strong().color(Color32::WHITE));
        ui.add_space(14.0);

        // Error frame — same colours as original error banner
        egui::Frame::new()
            .fill(Color32::from_rgb(60, 25, 25))
            .stroke(Stroke::new(1.0, RED_DIM))
            .corner_radius(egui::CornerRadius::same(4))
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&display).size(11.0).color(RED_DIM));
            });

        ui.add_space(14.0);

        let dismiss = egui::Button::new(
            RichText::new("Dismiss").size(11.0).color(DARK_TEXT_DIM),
        )
        .stroke(Stroke::new(1.0, DARK_BORDER))
        .fill(DARK_BG_2)
        .min_size(egui::vec2(ui.available_width(), 28.0));

        if ui.add(dismiss).clicked() {
            cmd.push(EditorCommand::ClearEncodeStatus);
        }
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
        let name_resp = ui.add_enabled(
            !is_encoding,
            egui::TextEdit::singleline(&mut self.filename)
                .desired_width(f32::INFINITY)
                .hint_text("filename…"),
        );
        // Consume Enter so Windows doesn't play the system beep when the user
        // confirms the field. The TextEdit handles the key internally but
        // doesn't mark it consumed in egui's event queue.
        if name_resp.has_focus() {
            ui.input_mut(|i| i.events.retain(|e| {
                !matches!(e, egui::Event::Key { key: egui::Key::Enter, pressed: true, .. })
            }));
        }

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

        // ── Transitions ───────────────────────────────────────────────────────
        // Transitions are set per clip boundary on the timeline — click the
        // ✂ badge between any two touching clips to add a dissolve.
        egui::Frame::new()
            .fill(DARK_BG_3)
            .stroke(Stroke::new(1.0, DARK_BORDER))
            .corner_radius(egui::CornerRadius::same(4))
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let timeline_ids: std::collections::HashSet<_> =
                    state.timeline.iter().map(|c| c.id).collect();
                let transition_count = state.transitions.iter()
                    .filter(|t| t.kind.kind != velocut_core::transitions::TransitionKind::Cut
                        && timeline_ids.contains(&t.after_clip_id))
                    .count();
                if transition_count == 0 {
                    ui.label(
                        RichText::new("No transitions set")
                            .size(11.0).color(DARK_TEXT_DIM),
                    );
                } else {
                    ui.label(
                        RichText::new(format!("🔗  {} transition{} active",
                            transition_count,
                            if transition_count == 1 { "" } else { "s" }))
                            .size(11.0).color(ACCENT),
                    );
                }
                ui.add_space(2.0);
                ui.label(
                    RichText::new("Click ✂ between clips on timeline to edit")
                        .size(10.0).color(DARK_TEXT_DIM),
                );
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