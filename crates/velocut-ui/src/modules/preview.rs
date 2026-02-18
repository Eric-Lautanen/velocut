// crates/velocut-ui/src/modules/preview.rs
use super::EditorModule;
use velocut_core::state::{ProjectState, AspectRatio};
use velocut_core::commands::EditorCommand;
use crate::modules::ThumbnailCache;
use crate::theme::{ACCENT, DARK_BG_2, DARK_BG_3, DARK_BORDER};
use egui::{Ui, Color32, Sense, Rect, Pos2, Stroke, RichText};

pub struct PreviewModule;

impl EditorModule for PreviewModule {
    fn name(&self) -> &str { "Preview" }

    fn ui(&mut self, ui: &mut Ui, state: &ProjectState, thumb_cache: &mut ThumbnailCache, cmd: &mut Vec<EditorCommand>) {
        ui.vertical(|ui| {
            // ── Header ───────────────────────────────────────────────────────
            egui::Frame::new()
                .fill(DARK_BG_2)
                .inner_margin(egui::Margin { left: 8, right: 8, top: 5, bottom: 5 })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("📺 Monitor").size(12.0).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let mut ar = state.aspect_ratio;
                            egui::ComboBox::from_id_salt("aspect_ratio")
                                .selected_text(match ar {
                                    AspectRatio::SixteenNine   => "16:9",
                                    AspectRatio::NineSixteen   => "9:16",
                                    AspectRatio::TwoThree      => "2:3",
                                    AspectRatio::ThreeTwo      => "3:2",
                                    AspectRatio::FourThree     => "4:3",
                                    AspectRatio::OneOne        => "1:1",
                                    AspectRatio::FourFive      => "4:5",
                                    AspectRatio::TwentyOneNine => "21:9",
                                    AspectRatio::Anamorphic    => "2.39:1",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut ar, AspectRatio::SixteenNine,   "16:9 — YouTube / HD");
                                    ui.selectable_value(&mut ar, AspectRatio::NineSixteen,   "9:16 — TikTok / Reels");
                                    ui.selectable_value(&mut ar, AspectRatio::TwoThree,      "2:3 — Portrait Photo");
                                    ui.selectable_value(&mut ar, AspectRatio::ThreeTwo,      "3:2 — Landscape Photo");
                                    ui.selectable_value(&mut ar, AspectRatio::FourThree,     "4:3 — Classic TV");
                                    ui.selectable_value(&mut ar, AspectRatio::OneOne,        "1:1 — Square");
                                    ui.selectable_value(&mut ar, AspectRatio::FourFive,      "4:5 — Instagram Portrait");
                                    ui.selectable_value(&mut ar, AspectRatio::TwentyOneNine, "21:9 — Ultrawide");
                                    ui.selectable_value(&mut ar, AspectRatio::Anamorphic,    "2.39:1 — Anamorphic");
                                });
                            if ar != state.aspect_ratio {
                                cmd.push(EditorCommand::SetAspectRatio(ar));
                            }
                        });
                    });
                });

            ui.add_space(4.0);

            // ── Video Canvas (centered, aspect-ratio correct) ───────────────
            let ratio      = state.active_video_ratio();
            let panel_w    = ui.available_width();
            let controls_h = 48.0;
            let panel_h    = (ui.available_height() - controls_h - 8.0).max(80.0);

            let (canvas_w, canvas_h) = {
                let w = panel_w;
                let h = w / ratio;
                if h <= panel_h { (w, h) } else { (panel_h * ratio, panel_h) }
            };

            let (outer_rect, _) = ui.allocate_exact_size(
                egui::vec2(panel_w, canvas_h), Sense::hover());
            let rect = egui::Rect::from_center_size(
                outer_rect.center(), egui::vec2(canvas_w, canvas_h));
            let painter   = ui.painter();

            if state.is_playing {
                painter.rect_stroke(rect.expand(2.0), 4,
                    Stroke::new(1.5, ACCENT.gamma_multiply(0.6)),
                    egui::StrokeKind::Outside);
            } else {
                painter.rect_stroke(rect.expand(1.0), 4,
                    Stroke::new(1.0, DARK_BORDER),
                    egui::StrokeKind::Outside);
            }
            painter.rect_filled(rect, 3.0, Color32::BLACK);

            let draw_tex = |painter: &egui::Painter,
                            rect: Rect,
                            tex: &egui::TextureHandle,
                            tint: Color32| -> Rect {
                painter.image(tex.id(), rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
                rect
            };

            let current_clip = state.timeline.iter().find(|c| {
                state.current_time >= c.start_time
                    && state.current_time < c.start_time + c.duration
            });

            if let Some(clip) = current_clip {
                if let Some(media) = state.library.iter().find(|m| m.id == clip.media_id) {
                    if let Some(tex) = thumb_cache.get(&media.id) {
                        let img_rect = draw_tex(&painter, rect, tex, Color32::WHITE);

                        let lp  = Pos2::new(rect.min.x + 6.0, rect.max.y - 18.0);
                        let lw  = media.name.len() as f32 * 6.8 + 8.0;
                        painter.rect_filled(
                            Rect::from_min_size(lp - egui::vec2(3.0, 2.0), egui::vec2(lw, 16.0)),
                            2.0, Color32::from_black_alpha(160));
                        painter.text(lp, egui::Align2::LEFT_TOP, &media.name,
                            egui::FontId::proportional(11.0), Color32::from_gray(200));

                        if state.is_playing {
                            let bp = Pos2::new(img_rect.max.x - 42.0, img_rect.min.y + 6.0);
                            painter.rect_filled(
                                Rect::from_min_size(bp, egui::vec2(38.0, 15.0)),
                                3.0, Color32::from_rgb(200, 30, 30));
                            painter.text(bp + egui::vec2(19.0, 7.5),
                                egui::Align2::CENTER_CENTER,
                                "● LIVE",
                                egui::FontId::monospace(8.0),
                                Color32::WHITE);
                        }
                    } else {
                        painter.text(rect.center() - egui::vec2(0.0, 18.0),
                            egui::Align2::CENTER_CENTER,
                            format!("◼  {}", media.name),
                            egui::FontId::proportional(15.0),
                            Color32::from_gray(80));
                        let t   = ui.input(|i| i.time) as f32;
                        let cx  = rect.center() + egui::vec2(0.0, 22.0);
                        let r   = 14.0_f32;
                        painter.circle_stroke(cx, r, Stroke::new(2.0, Color32::from_gray(40)));
                        let a   = t * 3.5;
                        painter.line_segment(
                            [cx, cx + egui::vec2(a.cos() * r, a.sin() * r)],
                            Stroke::new(2.5, ACCENT));
                        ui.ctx().request_repaint();
                    }
                }
            } else {
                painter.text(rect.center(), egui::Align2::CENTER_CENTER,
                    "NO SIGNAL", egui::FontId::monospace(16.0),
                    Color32::from_gray(48));
                let mut y = rect.min.y;
                while y < rect.max.y {
                    painter.line_segment(
                        [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
                        Stroke::new(0.5, Color32::from_rgba_unmultiplied(255, 255, 255, 3)));
                    y += 4.0;
                }
            }

            ui.add_space(6.0);

            // ── Transport Bar (centered) ──────────────────────────────────────
            {
                let bar_w    = 560.0_f32;
                let bar_h    = 48.0_f32;
                let panel    = ui.max_rect();
                let center_x = panel.center().x;
                let bar_rect = egui::Rect::from_center_size(
                    egui::pos2(center_x, ui.cursor().top() + bar_h * 0.5),
                    egui::vec2(bar_w, bar_h),
                );
                ui.allocate_rect(bar_rect, egui::Sense::hover());

                let mut child = ui.new_child(egui::UiBuilder::new()
                    .max_rect(bar_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)));

                egui::Frame::new()
                    .fill(DARK_BG_3)
                    .stroke(Stroke::new(1.0, DARK_BORDER))
                    .corner_radius(egui::CornerRadius::same(6))
                    .inner_margin(egui::Margin::same(8))
                    .show(&mut child, |ui| {
                    ui.set_width(bar_w - 16.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("⏮").size(14.0)).clicked() {
                            cmd.push(EditorCommand::Stop);
                        }
                        let play_lbl = if state.is_playing {
                            RichText::new("⏸").size(18.0).color(ACCENT)
                        } else {
                            RichText::new("▶").size(18.0).color(ACCENT)
                        };
                        if ui.button(play_lbl).clicked() {
                            if state.is_playing { cmd.push(EditorCommand::Pause); }
                            else               { cmd.push(EditorCommand::Play);  }
                        }
                        if ui.button(RichText::new("⏹").size(14.0)).clicked() {
                            cmd.push(EditorCommand::Stop);
                        }

                        ui.add_space(24.0);

                        let t      = state.current_time;
                        let mins   = (t / 60.0) as u32;
                        let secs   = (t % 60.0) as u32;
                        let frames = ((t * 30.0) as u32) % 30;
                        ui.label(
                            RichText::new(format!("{mins:02}:{secs:02}:{frames:02}"))
                                .monospace().size(12.0).color(ACCENT));

                        ui.add_space(24.0);

                        let total = state.timeline.iter()
                            .map(|c| c.start_time + c.duration)
                            .fold(0.0_f64, f64::max).max(1.0);
                        let progress = (state.current_time / total).clamp(0.0, 1.0) as f32;
                        ui.add(egui::ProgressBar::new(progress)
                            .desired_width(280.0)
                            .fill(ACCENT));

                        ui.add_space(24.0);

                        let mute_label = if state.muted { "🔇" } else {
                            if state.volume > 0.5 { "🔊" } else { "🔉" }
                        };
                        let mute_btn = egui::Button::new(
                            RichText::new(mute_label).size(14.0)
                                .color(if state.muted { DARK_BORDER } else { ACCENT })
                        ).frame(false);
                        if ui.add(mute_btn).on_hover_text("Toggle mute").clicked() {
                            cmd.push(EditorCommand::ToggleMute);
                        }

                        // Volume slider — needs local mut copy; emit command on change
                        let mut vol = state.volume;
                        ui.add_enabled_ui(!state.muted, |ui| {
                            let changed = ui.add_sized(
                                [90.0, 16.0],
                                egui::Slider::new(&mut vol, 0.0..=1.0)
                                    .show_value(false)
                                    .trailing_fill(true),
                            ).on_hover_text(format!("Volume: {}%", (state.volume * 100.0) as u32))
                            .changed();
                            if changed { cmd.push(EditorCommand::SetVolume(vol)); }
                        });
                    });
                });
            } // transport bar block
        });
    }
}