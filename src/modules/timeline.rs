// src/modules/timeline.rs
use super::EditorModule;
use crate::state::{ProjectState, ClipType};
use crate::modules::ThumbnailCache;
use crate::theme::{ACCENT, CLIP_VIDEO, CLIP_AUDIO, CLIP_SELECTED, DARK_BG_0, DARK_BORDER, DARK_BG_2};
use egui::{Ui, Color32, Rect, Pos2, Sense, Stroke, Align2, FontId, Vec2, Id, RichText};
use uuid::Uuid;

pub struct TimelineModule;

impl EditorModule for TimelineModule {
    fn name(&self) -> &str { "Timeline" }

    fn ui(&mut self, ui: &mut Ui, state: &mut ProjectState, thumb_cache: &mut ThumbnailCache) {
        // Auto-clear save status after 3 seconds
        if state.save_status.is_some() {
            let t = ui.input(|i| i.time);
            ui.memory_mut(|mem| {
                let key = egui::Id::new("save_status_time");
                let start = mem.data.get_temp_mut_or_insert_with(key, || t);
                if t - *start > 3.0 {
                    state.save_status = None;
                    mem.data.remove::<f64>(key);
                }
            });
            ui.ctx().request_repaint();
        } else {
            ui.memory_mut(|mem| mem.data.remove::<f64>(egui::Id::new("save_status_time")));
        }
        // Delete selected timeline clip
        if ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
            if state.selected_timeline_clip.is_some() {
                state.delete_selected();
            }
        }
        // Space = play/pause (timeline panel focused)
        if ui.input(|i| i.key_pressed(egui::Key::Space)) {
            let total = state.total_duration();
            if !state.is_playing && total > 0.0 && state.current_time >= total - 0.1 {
                state.current_time = 0.0;
            }
            state.is_playing = !state.is_playing;
        }
        // J/K/L scrubbing
        if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
            state.current_time = (state.current_time - 1.0 / 30.0).max(0.0);
            state.is_playing = false;
        }
        if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
            let total = state.total_duration();
            state.current_time = (state.current_time + 1.0 / 30.0).min(total.max(0.0));
            state.is_playing = false;
        }

        ui.vertical(|ui| {
            // ── Toolbar ──────────────────────────────────────────────────────
            egui::Frame::new()
                .fill(DARK_BG_2)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.group(|ui| {
                            if ui.button(if state.is_playing { "⏸" } else { "▶" }).clicked() {
                                let total = state.total_duration();
                                if !state.is_playing && total > 0.0 && state.current_time >= total - 0.1 {
                                    state.current_time = 0.0;
                                }
                                state.is_playing = !state.is_playing;
                            }
                            if ui.button("⏹").clicked() {
                                state.is_playing   = false;
                                state.current_time = 0.0;
                            }
                        });
                        ui.group(|ui| {
                            if ui.add_enabled(
                                state.selected_timeline_clip.is_some(),
                                egui::Button::new("🗑 Delete"),
                            ).clicked() {
                                state.delete_selected();
                            }
                        });
                        ui.group(|ui| {
                            // Enabled whenever the playhead sits over a video clip
                            let active_clip = state.timeline.iter().find(|c| {
                                state.current_time >= c.start_time
                                    && state.current_time < c.start_time + c.duration
                            }).and_then(|tc| {
                                state.library.iter().find(|l| l.id == tc.media_id)
                                    .map(|lib| (lib.path.clone(), state.current_time - tc.start_time + tc.source_offset))
                            });
                            if ui.add_enabled(
                                active_clip.is_some(),
                                egui::Button::new("📷 Save Frame"),
                            ).on_hover_text("Save current frame as PNG").clicked() {
                                if let Some((path, ts)) = active_clip {
                                    state.pending_save_pick = Some((path, ts));
                                }
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("🔍+").clicked() {
                                state.timeline_zoom = (state.timeline_zoom * 1.25).min(500.0);
                            }
                            if ui.button("🔍-").clicked() {
                                state.timeline_zoom = (state.timeline_zoom * 0.8).max(10.0);
                            }
                            ui.label(format!("Zoom: {:.0}px/s", state.timeline_zoom));
                            ui.separator();
                            if let Some(msg) = &state.save_status {
                                ui.label(
                                    egui::RichText::new(msg).size(10.0)
                                        .color(egui::Color32::from_rgb(100, 220, 100))
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new("Space=Play  Del=Remove  ←/→=Frame")
                                        .size(9.0).color(egui::Color32::from_gray(80))
                                );
                            }
                        });
                    });
                });

            ui.separator();

            egui::ScrollArea::horizontal()
                .id_salt("timeline_scroll")
                .show(ui, |ui| {
                    let track_height  = 54.0_f32;
                    let track_gap     = 4.0_f32;
                    let header_height = 28.0_f32;
                    let num_tracks    = 4_usize;

                    let max_time = state.total_duration().max(60.0);
                    let total_w  = (max_time as f32 * state.timeline_zoom) + 300.0;
                    let total_h  = header_height + (track_height + track_gap) * num_tracks as f32;

                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(total_w, total_h), Sense::click_and_drag());
                    let painter = ui.painter();

                    painter.rect_filled(rect, 0.0, DARK_BG_0);

                    // Track lanes
                    for t in 0..num_tracks {
                        let y    = rect.min.y + header_height + t as f32 * (track_height + track_gap);
                        let lane = Rect::from_min_size(Pos2::new(rect.min.x, y),
                            egui::vec2(rect.width(), track_height));
                        painter.rect_filled(lane, 0.0,
                            if t % 2 == 0 { Color32::from_rgba_unmultiplied(255, 255, 255, 3) }
                            else { Color32::TRANSPARENT });
                        let label = match t { 0 => "V1", 1 => "A1", 2 => "V2", _ => "A2" };
                        painter.text(Pos2::new(rect.min.x + 4.0, y + track_height * 0.5),
                            Align2::LEFT_CENTER, label, FontId::monospace(9.0),
                            Color32::from_rgba_unmultiplied(120, 120, 138, 180));
                    }

                    // Ruler
                    painter.rect_filled(
                        Rect::from_min_size(rect.min, egui::vec2(rect.width(), header_height)),
                        0.0, Color32::from_rgb(16, 16, 20));
                    let step = ruler_step(state.timeline_zoom);
                    let mut s = 0.0f64;
                    while s <= max_time + step {
                        let x        = rect.min.x + (s as f32 * state.timeline_zoom);
                        let is_major = (s % (step * 5.0)).abs() < step * 0.1;
                        let tick_h   = if is_major { header_height } else { header_height * 0.4 };
                        painter.line_segment(
                            [Pos2::new(x, rect.min.y + header_height - tick_h),
                             Pos2::new(x, rect.min.y + header_height)],
                            Stroke::new(1.0, Color32::from_gray(60)));
                        if is_major {
                            painter.text(Pos2::new(x + 3.0, rect.min.y + 4.0),
                                Align2::LEFT_TOP, format_time(s), FontId::monospace(10.0),
                                Color32::from_gray(140));
                        }
                        s += step;
                    }

                    // DnD drop zone
                    let payload: Option<Uuid> = ui.memory(|m| m.data.get_temp(Id::new("DND_PAYLOAD")));
                    let content_rect = Rect::from_min_max(
                        Pos2::new(rect.min.x, rect.min.y + header_height), rect.max);

                    if let Some(clip_id) = payload {
                        if let Some(hover) = ui.input(|i| i.pointer.hover_pos()) {
                            if content_rect.contains(hover) {
                                let raw_t = ((hover.x - rect.min.x) / state.timeline_zoom).max(0.0) as f64;
                                let mut snapped = raw_t;
                                if snapped < 0.5 { snapped = 0.0; }
                                let row = {
                                    let rel_y = hover.y - (rect.min.y + header_height);
                                    ((rel_y / (track_height + track_gap)) as usize).min(num_tracks - 1)
                                };
                                let track_end: f64 = state.timeline.iter()
                                    .filter(|c| c.track_row == row)
                                    .map(|c| c.start_time + c.duration)
                                    .fold(f64::NEG_INFINITY, f64::max);
                                if track_end.is_finite() && (snapped - track_end).abs() < 1.0 {
                                    snapped = track_end;
                                }
                                let line_x = rect.min.x + snapped as f32 * state.timeline_zoom;
                                let snapping = track_end.is_finite() && (raw_t - track_end).abs() < 1.0;
                                painter.line_segment(
                                    [Pos2::new(line_x, content_rect.min.y),
                                     Pos2::new(line_x, content_rect.max.y)],
                                    Stroke::new(2.0, if snapping {
                                        Color32::from_rgb(255, 200, 50)
                                    } else { ACCENT }));

                                if ui.input(|i| i.pointer.any_released()) {
                                    state.add_to_timeline(clip_id, snapped);
                                    ui.memory_mut(|mem| mem.data.remove::<Uuid>(Id::new("DND_PAYLOAD")));
                                }
                            }
                        }
                    }

                    // ── Timeline Clips ─────────────────────────────────────────
                    let clips: Vec<_> = state.timeline.iter().cloned().collect();
                    let mut to_delete: Option<Uuid> = None;

                    for clip in &clips {
                        let lib       = state.library.iter().find(|l| l.id == clip.media_id);
                        let media_name = lib.map(|l| l.name.as_str()).unwrap_or("Unknown");
                        let clip_type  = lib.map(|l| l.clip_type).unwrap_or(ClipType::Video);
                        let waveform   = lib.map(|l| l.waveform_peaks.as_slice()).unwrap_or(&[]);

                        let start_x = rect.min.x + (clip.start_time as f32 * state.timeline_zoom);
                        let width   = (clip.duration as f32 * state.timeline_zoom).max(4.0);
                        let y_off   = header_height + clip.track_row as f32 * (track_height + track_gap);

                        let clip_rect = Rect::from_min_size(
                            Pos2::new(start_x, rect.min.y + y_off),
                            egui::vec2(width, track_height));

                        let is_selected = state.selected_timeline_clip == Some(clip.id);
                        let body_color  = if is_selected { CLIP_SELECTED }
                            else if clip_type == ClipType::Audio { CLIP_AUDIO }
                            else { CLIP_VIDEO };

                        painter.rect_filled(clip_rect, 4.0, body_color);

                        // ── Thumbnail strip on video clips ──────────────────
                        if clip_type == ClipType::Video && width > 20.0 {
                            if let Some(media) = lib {
                                if let Some(tex) = thumb_cache.get(&media.id) {
                                    // Tile the thumbnail across the clip width
                                    let tex_aspect = tex.size_vec2().x / tex.size_vec2().y.max(1.0);
                                    let tile_w = (track_height * tex_aspect).min(width);
                                    let mut tx_start = clip_rect.min.x;
                                    let uv = egui::Rect::from_min_max(
                                        egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0));
                                    while tx_start < clip_rect.max.x {
                                        let tile_end = (tx_start + tile_w).min(clip_rect.max.x);
                                        let uv_frac = (tile_end - tx_start) / tile_w;
                                        let tile_rect = Rect::from_min_max(
                                            Pos2::new(tx_start, clip_rect.min.y + 3.0),
                                            Pos2::new(tile_end, clip_rect.max.y));
                                        let tile_uv = egui::Rect::from_min_max(
                                            egui::Pos2::ZERO, egui::Pos2::new(uv_frac, uv.max.y));
                                        painter.image(tex.id(), tile_rect, tile_uv,
                                            Color32::from_rgba_unmultiplied(255, 255, 255, 120));
                                        tx_start += tile_w;
                                        if tile_w <= 0.0 { break; }
                                    }
                                }
                            }
                        }

                        // Waveform overlay
                        if !waveform.is_empty() && width > 10.0 {
                            draw_waveform(&painter, clip_rect, waveform, clip_type);
                        }

                        // Top stripe
                        let stripe_color = if is_selected { ACCENT }
                            else if clip_type == ClipType::Audio { Color32::from_rgb(80, 200, 140) }
                            else { Color32::from_rgb(100, 140, 220) };
                        painter.rect_filled(
                            Rect::from_min_size(clip_rect.min, egui::vec2(clip_rect.width(), 3.0)),
                            egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 }, stripe_color);

                        // Border
                        painter.rect_stroke(clip_rect, 4,
                            Stroke::new(if is_selected { 1.5 } else { 1.0 },
                                if is_selected { ACCENT } else { DARK_BORDER }),
                            egui::StrokeKind::Outside);

                        // Name label
                        if width > 30.0 {
                            // Dark pill behind text for readability over thumbnails
                            let label_pos = clip_rect.min + Vec2::new(6.0, 8.0);
                            painter.text(label_pos, Align2::LEFT_TOP, media_name,
                                FontId::proportional(11.0),
                                Color32::from_rgba_unmultiplied(255, 255, 255, 220));
                        }

                        // Duration badge
                        if width > 50.0 {
                            painter.text(clip_rect.right_bottom() - Vec2::new(4.0, 4.0),
                                Align2::RIGHT_BOTTOM, format!("{:.1}s", clip.duration),
                                FontId::monospace(9.0),
                                Color32::from_rgba_unmultiplied(255, 255, 255, 140));
                        }

                        // Click/drag to select and move
                        let clip_interact = ui.interact(clip_rect, Id::new(clip.id), Sense::click_and_drag());
                        if clip_interact.clicked() {
                            state.selected_timeline_clip = Some(clip.id);
                            state.selected_library_clip  = None;
                        }
                        if clip_interact.drag_started() {
                            state.selected_timeline_clip = Some(clip.id);
                            state.selected_library_clip  = None;
                        }
                        if clip_interact.dragged() {
                            let delta_t    = clip_interact.drag_delta().x as f64 / state.timeline_zoom as f64;
                            let snap_px    = 8.0_f64 / state.timeline_zoom as f64;
                            let clip_id    = clip.id;
                            let clip_row   = clip.track_row;
                            // Collect neighbor edges BEFORE mutable borrow
                            let neighbors: Vec<f64> = state.timeline.iter()
                                .filter(|c| c.id != clip_id && c.track_row == clip_row)
                                .flat_map(|c| [c.start_time, c.start_time + c.duration])
                                .collect();
                            if let Some(tc) = state.timeline.iter_mut().find(|c| c.id == clip_id) {
                                tc.start_time = (tc.start_time + delta_t).max(0.0);
                                // Snap to zero
                                if tc.start_time < snap_px {
                                    tc.start_time = 0.0;
                                } else {
                                    // Snap to neighbor edges
                                    for edge in &neighbors {
                                        if (tc.start_time - edge).abs() < snap_px {
                                            tc.start_time = *edge;
                                            break;
                                        }
                                    }
                                }
                            }
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                        } else if clip_interact.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                        }

                        // Right-click context menu
                        clip_interact.context_menu(|ui| {
                            ui.set_min_width(160.0);
                            let clip_id = clip.id;
                            if ui.button("🗑  Remove from timeline").clicked() {
                                to_delete = Some(clip_id);
                                ui.close();
                            }
                            ui.separator();
                            ui.label(RichText::new(media_name).size(10.0)
                                .color(egui::Color32::from_gray(120)));
                            ui.label(RichText::new(format!("Duration: {:.2}s", clip.duration))
                                .size(10.0).color(egui::Color32::from_gray(100)));
                        });
                    }

                    if let Some(del_id) = to_delete {
                        state.timeline.retain(|c| c.id != del_id);
                        if state.selected_timeline_clip == Some(del_id) {
                            state.selected_timeline_clip = None;
                        }
                    }

                    // Playhead — clamped to total duration
                    let total_duration = state.total_duration();
                    let clamped_time = if total_duration > 0.0 {
                        state.current_time.min(total_duration)
                    } else {
                        state.current_time
                    };
                    let ph_x = rect.min.x + (clamped_time as f32 * state.timeline_zoom);
                    painter.line_segment(
                        [Pos2::new(ph_x + 1.0, rect.min.y), Pos2::new(ph_x + 1.0, rect.max.y)],
                        Stroke::new(1.0, Color32::from_black_alpha(60)));
                    painter.line_segment(
                        [Pos2::new(ph_x, rect.min.y), Pos2::new(ph_x, rect.max.y)],
                        Stroke::new(2.0, ACCENT));
                    painter.add(egui::Shape::convex_polygon(
                        vec![Pos2::new(ph_x - 6.0, rect.min.y),
                             Pos2::new(ph_x + 6.0, rect.min.y),
                             Pos2::new(ph_x, rect.min.y + 12.0)],
                        ACCENT, Stroke::NONE));

                    // Scrub on drag over empty area (not while dragging a clip)
                    let dragging_clip = state.selected_timeline_clip
                        .map(|id| ui.ctx().is_being_dragged(Id::new(id)))
                        .unwrap_or(false);
                    if response.dragged() && !dragging_clip {
                        if let Some(ptr) = response.interact_pointer_pos() {
                            let t = ((ptr.x - rect.min.x) / state.timeline_zoom).max(0.0) as f64;
                            state.current_time = t;
                            state.is_playing   = false;
                        }
                    }
                });
        });
    }
}

fn draw_waveform(painter: &egui::Painter, clip_rect: Rect, peaks: &[f32], clip_type: ClipType) {
    if peaks.is_empty() { return; }
    let w    = clip_rect.width();
    let h    = clip_rect.height();
    let mid_y = clip_rect.min.y + h * 0.5;
    let visible = (w as usize).min(peaks.len()).max(1);
    let step    = peaks.len() as f32 / visible as f32;
    let wave_color = if clip_type == ClipType::Audio {
        Color32::from_rgba_unmultiplied(80, 220, 150, 160)
    } else {
        Color32::from_rgba_unmultiplied(180, 210, 255, 80)
    };
    for i in 0..visible {
        let idx  = ((i as f32 * step) as usize).min(peaks.len() - 1);
        let peak = peaks[idx];
        let half = peak * (h * 0.44);
        let x    = clip_rect.min.x + i as f32;
        painter.line_segment(
            [Pos2::new(x, mid_y - half), Pos2::new(x, mid_y + half)],
            Stroke::new(1.0, wave_color));
    }
}

fn ruler_step(zoom: f32) -> f64 {
    if zoom >= 200.0 { 0.5 }
    else if zoom >= 80.0  { 1.0 }
    else if zoom >= 30.0  { 5.0 }
    else { 10.0 }
}

fn format_time(s: f64) -> String {
    let m  = (s / 60.0) as u32;
    let sc = (s % 60.0) as u32;
    let fr = ((s * 30.0) as u32) % 30;
    format!("{m:02}:{sc:02}:{fr:02}")
}