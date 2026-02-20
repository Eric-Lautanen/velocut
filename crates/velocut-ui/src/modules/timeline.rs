// crates/velocut-ui/src/modules/timeline.rs
use super::EditorModule;
use velocut_core::state::{ProjectState, ClipType};
use velocut_core::commands::EditorCommand;
use velocut_core::helpers::time::format_time;
use velocut_core::transitions::TransitionType;
use crate::helpers::clip_query;
use crate::modules::ThumbnailCache;
use crate::theme::{ACCENT, CLIP_VIDEO, CLIP_AUDIO, CLIP_SELECTED, DARK_BG_0, DARK_BG_2, DARK_BG_3, DARK_BORDER, DARK_TEXT_DIM};
use egui::{Ui, Color32, Rect, Pos2, Sense, Stroke, Align2, FontId, Vec2, Id, RichText};
use uuid::Uuid;

pub struct TimelineModule {
    /// Which clip ID's outgoing transition popup is open, and where to show it.
    transition_popup: Option<(Uuid, Pos2)>,
    /// True on the frame a transition popup is first opened — suppresses the
    /// click-outside-to-close check so the opening click doesn't immediately
    /// close the popup it just spawned.
    transition_popup_just_opened: bool,

    /// Volume popup: which clip's speaker badge was clicked, and the screen-space
    /// anchor position (bottom-center of the badge). Closed on click outside,
    /// exactly like transition_popup.
    vol_popup: Option<(Uuid, Pos2)>,
    /// True on the frame the vol popup is first opened — suppresses the
    /// click-outside-to-close check so the opening click doesn't immediately
    /// close the popup it just spawned.
    vol_popup_just_opened: bool,

    /// Last timeline position (seconds) for which a scrub decode was emitted.
    ///
    /// Used to deduplicate `SetPlayhead` commands during ruler and playhead-handle
    /// drags.  At low zoom levels many pixels of mouse movement map to sub-frame
    /// time deltas, firing redundant decode wakes and RGBA allocations.  We skip
    /// the emit when `|new_t - last_t| < 1/30 s` (one frame at 30 fps).
    ///
    /// Reset to a negative sentinel on construction.  Updated whenever a
    /// `SetPlayhead` is actually pushed so the filter stays tight.
    last_scrub_emitted_time: f64,
}

impl TimelineModule {
    pub fn new() -> Self {
        Self {
            transition_popup:             None,
            transition_popup_just_opened: false,
            vol_popup:                    None,
            vol_popup_just_opened:        false,
            last_scrub_emitted_time:      f64::NEG_INFINITY,
        }
    }
}

// ── Small styling helpers ──────────────────────────────────────────────────────
// These keep the toolbar code readable without a macro.

/// Standard toolbar button — consistent height, icon-forward.
fn tool_btn(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::new(label)
        .min_size(egui::vec2(0.0, 26.0))
}

/// Accented action button (Split) — subtle tinted fill so it reads as primary.
fn action_btn(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::new(label)
        .fill(Color32::from_rgb(35, 65, 105))
        .stroke(Stroke::new(1.0, Color32::from_rgb(80, 130, 210)))
        .min_size(egui::vec2(0.0, 26.0))
}

/// Playhead-frame export button — amber tint to distinguish it from the
/// neutral First/Last Frame buttons while staying in the same visual family.
fn playhead_btn(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    egui::Button::new(label)
        .fill(Color32::from_rgb(75, 50, 8))
        .stroke(Stroke::new(1.0, Color32::from_rgb(210, 148, 38)))
        .min_size(egui::vec2(0.0, 26.0))
}

impl EditorModule for TimelineModule {
    fn name(&self) -> &str { "Timeline" }

    fn ui(&mut self, ui: &mut Ui, state: &ProjectState, thumb_cache: &mut ThumbnailCache, cmd: &mut Vec<EditorCommand>) {
        // Auto-clear save status after 3 seconds (pure UI memory, no state mutation)
        if state.save_status.is_some() {
            let t = ui.input(|i| i.time);
            ui.memory_mut(|mem| {
                let key = egui::Id::new("save_status_time");
                let start = mem.data.get_temp_mut_or_insert_with(key, || t);
                if t - *start > 3.0 {
                    cmd.push(EditorCommand::ClearSaveStatus);
                    mem.data.remove::<f64>(key);
                }
            });
            ui.ctx().request_repaint();
        } else {
            ui.memory_mut(|mem| mem.data.remove::<f64>(egui::Id::new("save_status_time")));
        }

        // ── Keyboard shortcuts (only when no popup is open) ───────────────────
        if self.transition_popup.is_none() {
            if ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
                if let Some(id) = state.selected_timeline_clip {
                    cmd.push(EditorCommand::PushUndoSnapshot);
                    cmd.push(EditorCommand::DeleteTimelineClip(id));
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Space)) {
                if state.is_playing { cmd.push(EditorCommand::Pause); }
                else                { cmd.push(EditorCommand::Play);  }
            }
            if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                cmd.push(EditorCommand::Pause);
                cmd.push(EditorCommand::SetPlayhead((state.current_time - 1.0 / 30.0).max(0.0)));
            }
            if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                let total = state.total_duration();
                cmd.push(EditorCommand::Pause);
                cmd.push(EditorCommand::SetPlayhead((state.current_time + 1.0 / 30.0).min(total.max(0.0))));
            }
            // S — split clip at playhead
            if ui.input(|i| i.key_pressed(egui::Key::S)) {
                cmd.push(EditorCommand::PushUndoSnapshot);
                cmd.push(EditorCommand::SplitClipAt(state.current_time));
            }
            // Ctrl+Z — Undo
            if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Z)) {
                cmd.push(EditorCommand::Undo);
            }
            // Ctrl+Y or Ctrl+Shift+Z — Redo
            if ui.input(|i| i.modifiers.ctrl &&
                (i.key_pressed(egui::Key::Y) ||
                 (i.modifiers.shift && i.key_pressed(egui::Key::Z))))
            {
                cmd.push(EditorCommand::Redo);
            }
        }

        ui.vertical(|ui| {
            // ── Toolbar ──────────────────────────────────────────────────────
            egui::Frame::new()
                .fill(DARK_BG_2)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {

                        // ── Playback controls ─────────────────────────────────
                        ui.group(|ui| {
                            if ui.add(tool_btn(if state.is_playing { "⏸" } else { "▶" })).clicked() {
                                if state.is_playing { cmd.push(EditorCommand::Pause); }
                                else               { cmd.push(EditorCommand::Play);  }
                            }
                            if ui.add(tool_btn("⏹")).clicked() {
                                cmd.push(EditorCommand::Stop);
                            }
                        });

                        // ── Clip ops ──────────────────────────────────────────
                        ui.group(|ui| {
                            if ui.add_enabled(
                                state.selected_timeline_clip.is_some(),
                                tool_btn("🗑 Delete"),
                            ).clicked() {
                                if let Some(id) = state.selected_timeline_clip {
                                    cmd.push(EditorCommand::PushUndoSnapshot);
                                    cmd.push(EditorCommand::DeleteTimelineClip(id));
                                }
                            }
                        });

                        // ── Frame extraction — always visible, enabled only when
                        //    a clip is selected and resolvable in the library. ──
                        ui.group(|ui| {
                            let extract_enabled = state.selected_timeline_clip.is_some()
                                && clip_query::selected_clip_library_entry(state).is_some();

                            if ui.add_enabled(extract_enabled, tool_btn("⏮ First Frame"))
                                .on_hover_text("Extract first frame of selected clip as PNG")
                                .on_disabled_hover_text("Select a timeline clip first")
                                .clicked()
                            {
                                if let Some(tc) = clip_query::selected_timeline_clip(state) {
                                    if let Some(lib) = clip_query::library_entry_for(state, tc) {
                                        cmd.push(EditorCommand::RequestSaveFramePicker {
                                            path: lib.path.clone(),
                                            timestamp: tc.source_offset,
                                        });
                                    }
                                }
                            }

                            if ui.add_enabled(extract_enabled, playhead_btn("🎯 This Frame"))
                                .on_hover_text("Export the exact frame under the playhead as PNG")
                                .on_disabled_hover_text("Select a timeline clip first")
                                .clicked()
                            {
                                if let Some((ts, lib)) = clip_query::playhead_source_timestamp(state) {
                                    cmd.push(EditorCommand::RequestSaveFramePicker {
                                        path: lib.path.clone(),
                                        timestamp: ts,
                                    });
                                }
                            }

                            if ui.add_enabled(extract_enabled, tool_btn("⏭ Last Frame"))
                                .on_hover_text("Extract last frame of selected clip as PNG")
                                .on_disabled_hover_text("Select a timeline clip first")
                                .clicked()
                            {
                                if let Some(tc) = clip_query::selected_timeline_clip(state) {
                                    if let Some(lib) = clip_query::library_entry_for(state, tc) {
                                        let ts = (tc.source_offset + tc.duration - 1.0 / 30.0).max(0.0);
                                        cmd.push(EditorCommand::RequestSaveFramePicker {
                                            path: lib.path.clone(),
                                            timestamp: ts,
                                        });
                                    }
                                }
                            }
                        });

                        // ── Extract Audio ─────────────────────────────────────
                        // Enabled when a video clip is selected and hasn't been
                        // extracted yet. Mutes the video clip's audio and drops
                        // a linked audio clip on the A track below it.
                        ui.group(|ui| {
                            let extract_enabled = clip_query::selected_timeline_clip(state)
                                .map(|tc| {
                                    !tc.audio_muted
                                        && clip_query::library_entry_for(state, tc)
                                            .map(|l| l.clip_type == velocut_core::state::ClipType::Video)
                                            .unwrap_or(false)
                                })
                                .unwrap_or(false);

                            if ui.add_enabled(extract_enabled, tool_btn("🎵 Extract Audio"))
                                .on_hover_text("Extract audio to track below  [splits video/audio]")
                                .on_disabled_hover_text("Select a video clip that hasn't been extracted yet")
                                .clicked()
                            {
                                if let Some(id) = state.selected_timeline_clip {
                                    cmd.push(EditorCommand::PushUndoSnapshot);
                                    cmd.push(EditorCommand::ExtractAudioTrack(id));
                                }
                            }
                        });
                        ui.group(|ui| {
                            let can_undo = state.undo_len > 0;
                            let can_redo = state.redo_len > 0;

                            if ui.add_enabled(can_undo, tool_btn("↩ Undo"))
                                .on_hover_text("Undo  [Ctrl+Z]")
                                .on_disabled_hover_text("Nothing to undo")
                                .clicked()
                            {
                                cmd.push(EditorCommand::Undo);
                            }
                            if ui.add_enabled(can_redo, tool_btn("↪ Redo"))
                                .on_hover_text("Redo  [Ctrl+Y]")
                                .on_disabled_hover_text("Nothing to redo")
                                .clicked()
                            {
                                cmd.push(EditorCommand::Redo);
                            }
                        });

                        // ── Split — accented, enabled when playhead is over a
                        //    splittable clip with > 2 frames on each side. ─────
                        {
                            let min_dur = 2.0 / 30.0;
                            let can_split = state.timeline.iter().any(|c| {
                                state.current_time > c.start_time + min_dur
                                    && state.current_time < c.start_time + c.duration - min_dur
                            });
                            ui.group(|ui| {
                                if ui.add_enabled(can_split, action_btn("✂ Split"))
                                    .on_hover_text("Split clip at playhead  [S]")
                                    .clicked()
                                {
                                    cmd.push(EditorCommand::PushUndoSnapshot);
                                    cmd.push(EditorCommand::SplitClipAt(state.current_time));
                                }
                            });
                        }

                        // ── Right side: zoom + status ─────────────────────────
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(tool_btn("🔍+")).clicked() {
                                cmd.push(EditorCommand::SetTimelineZoom((state.timeline_zoom * 1.25).min(500.0)));
                            }
                            if ui.add(tool_btn("🔍-")).clicked() {
                                cmd.push(EditorCommand::SetTimelineZoom((state.timeline_zoom * 0.8).max(10.0)));
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
                                    egui::RichText::new("Space=Play  Del=Remove  ⬅️➡️=Frame  S=Split  Ctrl+Z=Undo")
                                        .size(9.0).color(egui::Color32::from_gray(80))
                                );
                            }
                        });
                    });
                });

            ui.separator();


                egui::ScrollArea::both()
                    .id_salt("timeline_scroll")
                    .show(ui, |ui: &mut egui::Ui| {
                    let track_height  = 54.0_f32;
                    let track_gap     = 4.0_f32;
                    let header_height = 28.0_f32;
                    let num_tracks    = 4_usize;

                    let max_time = state.total_duration().max(60.0);
                    let total_w  = (max_time as f32 * state.timeline_zoom) + 300.0;
                    let total_h  = header_height + (track_height + track_gap) * num_tracks as f32;

                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(total_w, total_h), Sense::click());
                    // `.clone()` gives an owned Painter (egui Painter is Arc-backed)
                    // so ui is free for mutable calls like ui.put() later in the loop.
                    let painter = ui.painter().clone();

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

                    // Ruler click/drag → seek
                    let ruler_rect = Rect::from_min_size(rect.min, egui::vec2(rect.width(), header_height));
                    let ruler_resp = ui.interact(ruler_rect, Id::new("timeline_ruler"), Sense::click_and_drag());
                    if ruler_resp.clicked() || ruler_resp.dragged() {
                        if let Some(ptr) = ruler_resp.interact_pointer_pos() {
                            let t         = ((ptr.x - rect.min.x) / state.timeline_zoom).max(0.0) as f64;
                            let t_clamped = t.min(state.total_duration().max(0.0));
                            if ruler_resp.drag_started() || ruler_resp.clicked() {
                                // Click or drag-start: always emit so the user gets instant response.
                                cmd.push(EditorCommand::Pause);
                                cmd.push(EditorCommand::SetPlayhead(t_clamped));
                                self.last_scrub_emitted_time = t_clamped;
                            } else if (t_clamped - self.last_scrub_emitted_time).abs() >= 1.0 / 30.0 {
                                // Mid-drag: only emit when the cursor has moved at least one frame's
                                // worth of time.  At low zoom many pixels map to the same 1/30 s bucket
                                // — skipping them avoids flooding the decode thread with wakes that each
                                // allocate a full RGBA buffer and thrash the bucket cache.
                                cmd.push(EditorCommand::SetPlayhead(t_clamped));
                                self.last_scrub_emitted_time = t_clamped;
                            }
                        }
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    } else if ruler_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    }

                    // DnD drop zone
                    let payload: Option<Uuid> = ui.memory(|m| m.data.get_temp(Id::new("DND_PAYLOAD")));
                    let content_rect = Rect::from_min_max(
                        Pos2::new(rect.min.x, rect.min.y + header_height), rect.max);

                    if payload.is_some() && !ui.input(|i| i.pointer.any_down()) {
                        ui.memory_mut(|mem| mem.data.remove::<Uuid>(Id::new("DND_PAYLOAD")));
                    }

                    if let Some(clip_id) = payload {
                        // Resolve clip type for track enforcement before any hover logic.
                        let drag_clip_type = state.library.iter()
                            .find(|l| l.id == clip_id)
                            .map(|l| l.clip_type)
                            .unwrap_or(ClipType::Video);

                        if let Some(hover) = ui.input(|i| i.pointer.hover_pos()) {
                            if content_rect.contains(hover) {
                                let raw_t = ((hover.x - rect.min.x) / state.timeline_zoom).max(0.0) as f64;

                                // Raw row from hover y position.
                                let raw_row = {
                                    let rel_y = hover.y - (rect.min.y + header_height);
                                    ((rel_y / (track_height + track_gap)) as usize).min(num_tracks - 1)
                                };

                                // ── Track enforcement ──────────────────────────
                                // Video → even rows (V1=0, V2=2); Audio → odd rows (A1=1, A2=3).
                                let enforced_row = match drag_clip_type {
                                    ClipType::Video => {
                                        let r = if raw_row % 2 == 0 { raw_row } else { raw_row.saturating_sub(1) };
                                        r.min(2)
                                    }
                                    ClipType::Audio => {
                                        let r = if raw_row % 2 == 1 { raw_row } else { raw_row + 1 };
                                        r.min(3)
                                    }
                                };

                                let mut snapped = raw_t;
                                if snapped < 0.5 { snapped = 0.0; }
                                let track_end: f64 = state.timeline.iter()
                                    .filter(|c| c.track_row == enforced_row)
                                    .map(|c| c.start_time + c.duration)
                                    .fold(f64::NEG_INFINITY, f64::max);
                                if track_end.is_finite() && (snapped - track_end).abs() < 1.0 {
                                    snapped = track_end;
                                }

                                // Draw snap indicator only within the enforced lane.
                                let lane_y = rect.min.y + header_height
                                    + enforced_row as f32 * (track_height + track_gap);
                                let lane_rect = Rect::from_min_size(
                                    Pos2::new(rect.min.x, lane_y),
                                    egui::vec2(rect.width(), track_height));

                                let line_x   = rect.min.x + snapped as f32 * state.timeline_zoom;
                                let snapping = track_end.is_finite() && (raw_t - track_end).abs() < 1.0;

                                // Highlight the target lane so the user sees enforcement.
                                painter.rect_stroke(
                                    lane_rect, 
                                    0.0, 
                                    Stroke::new(1.0, ACCENT.linear_multiply(0.5)),
                                    egui::StrokeKind::Inside
                                );
                                painter.line_segment(
                                    [Pos2::new(line_x, lane_rect.min.y),
                                     Pos2::new(line_x, lane_rect.max.y)],
                                    Stroke::new(2.0, if snapping {
                                        Color32::from_rgb(255, 200, 50)
                                    } else { ACCENT }));

                                if ui.input(|i| i.pointer.any_released()) {
                                    cmd.push(EditorCommand::PushUndoSnapshot);
                                    cmd.push(EditorCommand::AddToTimeline {
                                        media_id:  clip_id,
                                        at_time:   snapped,
                                        track_row: enforced_row,
                                    });
                                    ui.memory_mut(|mem| mem.data.remove::<Uuid>(Id::new("DND_PAYLOAD")));
                                }
                            }
                        }
                    }

                    // ── Timeline Clips ─────────────────────────────────────────────
                    let mut to_delete: Option<Uuid> = None;

                    for clip in &state.timeline {
                        let lib        = clip_query::library_entry_for(state, clip);
                        let media_name = lib.map(|l| l.name.as_str()).unwrap_or("Unknown");
                        let clip_type  = lib.map(|l| l.clip_type).unwrap_or(ClipType::Video);
                        let waveform   = lib.map(|l| l.waveform_peaks.as_slice()).unwrap_or(&[]);

                        // A clip whose library entry is Video but which sits on an audio
                        // track row is an extracted-audio clip.  Give it audio rendering so
                        // it looks like an audio clip (green, waveform-only, no thumbnail).
                        let is_extracted_audio_clip = clip_query::is_extracted_audio_clip(clip);
                        let render_type = if is_extracted_audio_clip {
                            ClipType::Audio
                        } else {
                            clip_type
                        };

                        let start_x = rect.min.x + (clip.start_time as f32 * state.timeline_zoom);
                        let width   = (clip.duration as f32 * state.timeline_zoom).max(4.0);
                        let y_off   = header_height + clip.track_row as f32 * (track_height + track_gap);

                        let clip_rect = Rect::from_min_size(
                            Pos2::new(start_x, rect.min.y + y_off),
                            egui::vec2(width, track_height));

                        let is_selected = state.selected_timeline_clip == Some(clip.id);
                        let body_color  = if is_selected { CLIP_SELECTED }
                            else if render_type == ClipType::Audio { CLIP_AUDIO }
                            else { CLIP_VIDEO };

                        painter.rect_filled(clip_rect, 4.0, body_color);

                        // Thumbnail strip on video clips only — extracted audio clips
                        // sit on audio tracks and should show only waveform, not frames.
                        if render_type == ClipType::Video && width > 20.0 {
                            if let Some(media) = lib {
                                if let Some(tex) = thumb_cache.get(&media.id) {
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

                        // Waveform overlay.
                        // · Video clip with audio extracted (audio_muted=true): hide waveform —
                        //   the audio now lives on the extracted audio clip below.
                        // · Extracted audio clip (is_extracted_audio_clip): always show waveform.
                        // · Regular audio clip: always show waveform.
                        if !waveform.is_empty() && width > 10.0 && !clip.audio_muted {
                            draw_waveform(&painter, clip_rect, waveform, render_type, clip.volume);
                        }

                        // Top stripe
                        let stripe_color = if is_selected { ACCENT }
                            else if render_type == ClipType::Audio { Color32::from_rgb(80, 200, 140) }
                            else { Color32::from_rgb(100, 140, 220) };
                        painter.rect_filled(
                            Rect::from_min_size(clip_rect.min, egui::vec2(clip_rect.width(), 3.0)),
                            egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 }, stripe_color);

                        // Border
                        painter.rect_stroke(clip_rect, 4,
                            Stroke::new(if is_selected { 1.5 } else { 1.0 },
                                if is_selected { ACCENT } else { DARK_BORDER }),
                            egui::StrokeKind::Outside);

                        // Name label — capped to half the clip width so it never
                        // overflows into the duration badge or the right-hand clip.
                        if width > 30.0 {
                            let label_font = FontId::proportional(11.0);
                            let label_text = fit_label(media_name, width * 0.5);
                            let label_pos  = clip_rect.min + Vec2::new(6.0, 8.0);
                            painter.text(label_pos, Align2::LEFT_TOP, label_text,
                                label_font,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 220));
                        }


                        // Duration badge
                        if width > 50.0 {
                            painter.text(clip_rect.right_bottom() - Vec2::new(4.0, 4.0),
                                Align2::RIGHT_BOTTOM, format!("{:.1}s", clip.duration),
                                FontId::monospace(9.0),
                                Color32::from_rgba_unmultiplied(255, 255, 255, 140));
                        }

                        // ── Speaker / volume badge ─────────────────────────────
                        // Paint only here. ui.interact is registered AFTER
                        // clip_interact below so it wins the hit-test.
                        let vol_badge_geo: Option<(Rect, Pos2, bool)> = if width > 40.0 {
                            let badge_center = Pos2::new(
                                clip_rect.center().x,
                                clip_rect.max.y - 10.0,
                            );
                            let vol_is_open = self.vol_popup
                                .map(|(id, _)| id == clip.id)
                                .unwrap_or(false);
                            let badge_color = if vol_is_open { ACCENT } else { Color32::from_gray(165) };
                            let badge_rect  = Rect::from_center_size(badge_center, egui::vec2(18.0, 14.0));

                            painter.rect_filled(
                                badge_rect, 4.0,
                                badge_color.linear_multiply(if vol_is_open { 0.35 } else { 0.30 }),
                            );
                            painter.rect_stroke(
                                badge_rect, 4.0,
                                Stroke::new(1.0, badge_color),
                                egui::StrokeKind::Outside,
                            );
                            painter.text(
                                badge_center, Align2::CENTER_CENTER,
                                "🔊", FontId::proportional(9.0), badge_color,
                            );
                            Some((badge_rect, badge_center, vol_is_open))
                        } else {
                            None
                        };

                        // ── Trim handles ──────────────────────────────────────
                        // 7px interactive strips at each clip edge. Dragging the
                        // left edge adjusts source_offset + start_time (TrimClipStart);
                        // dragging the right edge adjusts duration (TrimClipEnd).
                        // Interacted before the body so they take priority for hover.
                        let trim_w = 7.0_f32;
                        let left_trim_rect = Rect::from_min_size(
                            clip_rect.min, egui::vec2(trim_w, track_height));
                        let right_trim_rect = Rect::from_min_max(
                            Pos2::new(clip_rect.max.x - trim_w, clip_rect.min.y), clip_rect.max);

                        let left_trim  = ui.interact(left_trim_rect,
                            Id::new(("trim_l", clip.id)), Sense::drag());
                        let right_trim = ui.interact(right_trim_rect,
                            Id::new(("trim_r", clip.id)), Sense::drag());
                        let is_trimming = left_trim.dragged() || right_trim.dragged();

                        if left_trim.hovered() || right_trim.hovered() || is_trimming {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        }

                        // Push undo snapshot once at the start of each trim drag —
                        // not every frame, so the stack stays clean.
                        if left_trim.drag_started() || right_trim.drag_started() {
                            cmd.push(EditorCommand::PushUndoSnapshot);
                        }

                        if left_trim.dragged() {
                            let delta = left_trim.drag_delta().x as f64 / state.timeline_zoom as f64;
                            // Clamp so source_offset never goes below 0 and duration stays > 2 frames.
                            let new_source_offset = (clip.source_offset + delta).max(0.0);
                            let actual_delta      = new_source_offset - clip.source_offset;
                            let new_duration      = (clip.duration - actual_delta).max(2.0 / 30.0);
                            cmd.push(EditorCommand::TrimClipStart {
                                id: clip.id, new_source_offset, new_duration });
                            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        }
                        if right_trim.dragged() {
                            let delta        = right_trim.drag_delta().x as f64 / state.timeline_zoom as f64;
                            let new_duration = (clip.duration + delta).max(2.0 / 30.0);
                            cmd.push(EditorCommand::TrimClipEnd { id: clip.id, new_duration });
                            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        }

                        // Paint trim handle affordance — subtle bright edges on hover/drag
                        let show_handles = left_trim.hovered() || right_trim.hovered() || is_trimming;
                        if show_handles || is_selected {
                            let handle_col = Color32::from_rgba_unmultiplied(255, 255, 255,
                                if is_trimming { 180 } else { 80 });
                            painter.rect_filled(left_trim_rect.shrink2(egui::vec2(2.0, 0.0)),
                                egui::CornerRadius { nw: 4, ne: 0, sw: 4, se: 0 }, handle_col);
                            painter.rect_filled(right_trim_rect.shrink2(egui::vec2(2.0, 0.0)),
                                egui::CornerRadius { nw: 0, ne: 4, sw: 0, se: 4 }, handle_col);
                        }

                        // ── Click / drag (body) ────────────────────────────────
                        // Click/drag interact: skipped while trimming so edges don't
                        // accidentally move the whole clip.
                        let clip_interact = ui.interact(clip_rect, Id::new(clip.id), Sense::click_and_drag());
                        if !is_trimming {
                            if clip_interact.clicked() {
                                cmd.push(EditorCommand::SelectTimelineClip(Some(clip.id)));
                                cmd.push(EditorCommand::SelectLibraryClip(None));
                                // Close transition popup when clicking a clip
                                self.transition_popup = None;
                            }
                            if clip_interact.drag_started() {
                                // Push undo snapshot once at start of move drag.
                                cmd.push(EditorCommand::PushUndoSnapshot);
                                cmd.push(EditorCommand::SelectTimelineClip(Some(clip.id)));
                                cmd.push(EditorCommand::SelectLibraryClip(None));
                                self.transition_popup = None;
                            }
                            if clip_interact.dragged() {
                                let delta_t = clip_interact.drag_delta().x as f64 / state.timeline_zoom as f64;
                                let snap_px = 8.0_f64 / state.timeline_zoom as f64;
                                let clip_id = clip.id;
                                let clip_row = clip.track_row;
                                let neighbors: Vec<f64> = state.timeline.iter()
                                    .filter(|c| c.id != clip_id && c.track_row == clip_row)
                                    .flat_map(|c| [c.start_time, c.start_time + c.duration])
                                    .collect();
                                let mut new_start = (clip.start_time + delta_t).max(0.0);
                                if new_start < snap_px {
                                    new_start = 0.0;
                                } else {
                                    for edge in &neighbors {
                                        if (new_start - edge).abs() < snap_px {
                                            new_start = *edge;
                                            break;
                                        }
                                    }
                                }
                                cmd.push(EditorCommand::MoveTimelineClip { id: clip_id, new_start });
                                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                            } else if clip_interact.hovered() && !left_trim.hovered() && !right_trim.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                            }
                        }

                        // Right-click context menu
                        clip_interact.context_menu(|ui: &mut egui::Ui| {
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

                        // ── Vol badge interact (registered after clip_interact so it wins) ──
                        if let Some((badge_rect, badge_center, vol_is_open)) = vol_badge_geo {
                            let clip_vol  = clip.volume;
                            let clip_id   = clip.id;
                            let badge_resp = ui.interact(
                                badge_rect,
                                Id::new(("vol_badge", clip_id)),
                                Sense::click(),
                            );
                            let badge_resp = badge_resp.on_hover_ui(|ui| {
                                let db = if clip_vol <= 0.0001 { -60.0_f32 }
                                    else { 20.0 * clip_vol.log10() };
                                let label = if db <= -59.0 {
                                    "Volume: -∞ dB  (click to adjust)".to_string()
                                } else {
                                    format!("Volume: {:+.1} dB  (click to adjust)", db)
                                };
                                ui.label(RichText::new(label).size(11.0));
                            });
                            if badge_resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            if badge_resp.clicked() {
                                if vol_is_open {
                                    self.vol_popup = None;
                                } else {
                                    cmd.push(EditorCommand::PushUndoSnapshot);
                                    self.vol_popup             = Some((clip_id, badge_center));
                                    self.vol_popup_just_opened = true;
                                }
                            }
                        }
                    }

                    // ── Transition badges ──────────────────────────────────────────
                    // For each track, find adjacent touching clip pairs and render a
                    // small clickable badge at the join point.
                    for track_row in 0..num_tracks {
                        let mut track_clips: Vec<_> = state.timeline.iter()
                            .filter(|c| c.track_row == track_row)
                            .collect();
                        track_clips.sort_by(|a, b| a.start_time.partial_cmp(&b.start_time).unwrap());

                        for i in 0..track_clips.len().saturating_sub(1) {
                            let clip_a = track_clips[i];
                            let clip_b = track_clips[i + 1];
                            let gap = clip_b.start_time - (clip_a.start_time + clip_a.duration);
                            if gap.abs() > 0.25 { continue; } // only touching clips

                            let join_x = rect.min.x
                                + ((clip_a.start_time + clip_a.duration) as f32 * state.timeline_zoom);
                            let y_off = header_height + track_row as f32 * (track_height + track_gap);
                            let badge_center = Pos2::new(
                                join_x,
                                rect.min.y + y_off + track_height * 0.5,
                            );

                            let current_kind = state.transitions.iter()
                                .find(|t| t.after_clip_id == clip_a.id)
                                .map(|t| &t.kind);

                            let has_transition = matches!(current_kind, Some(TransitionType::Crossfade { .. }));
                            let is_open = self.transition_popup.map(|(id, _)| id == clip_a.id).unwrap_or(false);

                            // ── Badge colour ──────────────────────────────────
                            // Open → accent; crossfade → blue; cut → clearly
                            // visible mid-gray (was gray(90) — too dark to see
                            // against the track background).
                            let badge_color = if is_open {
                                ACCENT
                            } else if has_transition {
                                Color32::from_rgb(120, 180, 255)
                            } else {
                                Color32::from_gray(165)  // was 90 — now visible
                            };

                            // Badge: small pill straddling the join line
                            let badge_rect = Rect::from_center_size(
                                badge_center,
                                egui::vec2(18.0, 18.0),
                            );
                            painter.rect_filled(
                                badge_rect,
                                5.0,
                                if is_open {
                                    badge_color.linear_multiply(0.35)
                                } else {
                                    badge_color.linear_multiply(0.30)  // was 0.18 — more fill
                                },
                            );
                            painter.rect_stroke(
                                badge_rect,
                                5.0,
                                Stroke::new(1.0, badge_color),
                                egui::StrokeKind::Outside,
                            );
                            // Icon: ⇌ for crossfade, ✂ for cut
                            let icon = if has_transition { "⇌" } else { "✂" };
                            painter.text(
                                badge_center,
                                Align2::CENTER_CENTER,
                                icon,
                                FontId::proportional(10.0),
                                badge_color,
                            );

                            // Hit area and interaction
                            let badge_sense = ui.interact(
                                badge_rect,
                                Id::new(("transition_badge", clip_a.id)),
                                Sense::click(),
                            );
                            let badge_sense = badge_sense.on_hover_ui(|ui: &mut egui::Ui| {
                                let tip = match current_kind {
                                    Some(TransitionType::Crossfade { duration_secs }) =>
                                        format!("Crossfade  {:.2}s — click to edit", duration_secs),
                                    _ => "Cut — click to add transition".to_string(),
                                };
                                ui.label(RichText::new(tip).size(11.0));
                            });
                            if badge_sense.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            if badge_sense.clicked() {
                                if is_open {
                                    self.transition_popup = None;
                                } else {
                                    self.transition_popup             = Some((clip_a.id, badge_center));
                                    self.transition_popup_just_opened = true;
                                }
                            }
                        }
                    }

                    if let Some(del_id) = to_delete {
                        cmd.push(EditorCommand::PushUndoSnapshot);
                        cmd.push(EditorCommand::DeleteTimelineClip(del_id));
                    }

                    // Playhead
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

                    // Playhead handle drag
                    let handle_rect = Rect::from_center_size(
                        Pos2::new(ph_x, rect.min.y + 6.0),
                        egui::vec2(16.0, 16.0),
                    );
                    let handle_resp = ui.interact(
                        handle_rect, Id::new("playhead_handle"), Sense::click_and_drag(),
                    );
                    if handle_resp.dragged() {
                        if let Some(ptr) = handle_resp.interact_pointer_pos() {
                            let t         = ((ptr.x - rect.min.x) / state.timeline_zoom).max(0.0) as f64;
                            let t_clamped = t.min(state.total_duration().max(0.0));
                            if handle_resp.drag_started() {
                                // Drag start: always emit so the handle feels immediately responsive.
                                cmd.push(EditorCommand::Pause);
                                cmd.push(EditorCommand::SetPlayhead(t_clamped));
                                self.last_scrub_emitted_time = t_clamped;
                            } else if (t_clamped - self.last_scrub_emitted_time).abs() >= 1.0 / 30.0 {
                                // Same dedup as ruler: skip sub-frame deltas during drag.
                                cmd.push(EditorCommand::SetPlayhead(t_clamped));
                                self.last_scrub_emitted_time = t_clamped;
                            }
                        }
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    } else if handle_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    }

                    // Background click = deselect + close popup
                    if response.clicked() {
                        cmd.push(EditorCommand::SelectTimelineClip(None));
                        cmd.push(EditorCommand::SelectLibraryClip(None));
                    }
                });

            // ── Transition popup ───────────────────────────────────────────────
            // Rendered outside the ScrollArea so it floats above everything.
            if let Some((after_clip_id, badge_pos)) = self.transition_popup {
                let current_kind = state.transitions.iter()
                    .find(|t| t.after_clip_id == after_clip_id)
                    .map(|t| t.kind.clone())
                    .unwrap_or(TransitionType::Cut);

                // Position the popup below and centered on the badge.
                // Clamp to avoid going off-screen bottom.
                let popup_pos = Pos2::new(badge_pos.x - 90.0, badge_pos.y + 16.0);

                let area_resp = egui::Area::new(Id::new("transition_popup_area"))
                    .fixed_pos(popup_pos)
                    .order(egui::Order::Foreground)
                    .interactable(true)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::new()
                            .fill(DARK_BG_3)
                            .stroke(Stroke::new(1.0, DARK_BORDER))
                            .corner_radius(egui::CornerRadius::same(6))
                            .inner_margin(egui::Margin::same(10))
                            .shadow(egui::Shadow {
                                offset: [0, 4],
                                blur: 12,
                                spread: 0,
                                color: Color32::from_black_alpha(120),
                            })
                            .show(ui, |ui| {
                                ui.set_min_width(180.0);

                                // Header
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("↔️  Transition")
                                            .size(12.0)
                                            .strong()
                                            .color(ACCENT),
                                    );
                                });
                                ui.add_space(6.0);
                                ui.separator();
                                ui.add_space(6.0);

                                // Type selector buttons
                                ui.horizontal(|ui| {
                                    let cut_selected = matches!(current_kind, TransitionType::Cut);
                                    let fade_selected = matches!(current_kind, TransitionType::Crossfade { .. });

                                    let cut_btn = egui::Button::new(
                                        RichText::new("✂  Cut")
                                            .size(11.0)
                                            .color(if cut_selected { Color32::BLACK } else { DARK_TEXT_DIM }),
                                    )
                                    .fill(if cut_selected { ACCENT } else { DARK_BG_2 })
                                    .stroke(Stroke::new(1.0, if cut_selected { ACCENT } else { DARK_BORDER }))
                                    .min_size(egui::vec2(80.0, 26.0));

                                    let fade_btn = egui::Button::new(
                                        RichText::new("🌫️  Dissolve")
                                            .size(11.0)
                                            .color(if fade_selected { Color32::BLACK } else { DARK_TEXT_DIM }),
                                    )
                                    .fill(if fade_selected { ACCENT } else { DARK_BG_2 })
                                    .stroke(Stroke::new(1.0, if fade_selected { ACCENT } else { DARK_BORDER }))
                                    .min_size(egui::vec2(80.0, 26.0));

                                    if ui.add(cut_btn).clicked() {
                                        cmd.push(EditorCommand::RemoveTransition(after_clip_id));
                                    }
                                    if ui.add(fade_btn).clicked() {
                                        let dur = match &current_kind {
                                            TransitionType::Crossfade { duration_secs } => *duration_secs,
                                            _ => 0.5,
                                        };
                                        cmd.push(EditorCommand::SetTransition {
                                            after_clip_id,
                                            kind: TransitionType::Crossfade { duration_secs: dur },
                                        });
                                    }
                                });

                                // Duration slider — only shown for Crossfade
                                if let TransitionType::Crossfade { duration_secs } = &current_kind {
                                    ui.add_space(8.0);
                                    ui.label(
                                        RichText::new("Duration").size(10.0).color(DARK_TEXT_DIM),
                                    );
                                    ui.add_space(2.0);
                                    let mut dur = *duration_secs;
                                    let slider = egui::Slider::new(&mut dur, 0.1f32..=3.0)
                                        .step_by(0.05)
                                        .suffix("s")
                                        .show_value(true);
                                    if ui.add(slider).changed() {
                                        cmd.push(EditorCommand::SetTransition {
                                            after_clip_id,
                                            kind: TransitionType::Crossfade { duration_secs: dur },
                                        });
                                    }
                                }
                            });
                    });

                // Close on click outside the popup — but skip the frame we just
                // opened it on, because the badge click that opened it would
                // immediately close it (badge is outside the popup rect).
                if !self.transition_popup_just_opened {
                    let clicked_somewhere = ui.input(|i| i.pointer.any_click());
                    if clicked_somewhere {
                        let click_pos = ui.input(|i| i.pointer.interact_pos());
                        if let Some(pos) = click_pos {
                            if !area_resp.response.rect.contains(pos) {
                                self.transition_popup = None;
                            }
                        }
                    }
                }
                self.transition_popup_just_opened = false;
            } else {
                // No popup open — ensure the flag is always cleared.
                self.transition_popup_just_opened = false;
            }

            // ── Volume popup ───────────────────────────────────────────────────
            // Floats above the speaker icon. Nearly transparent so the waveform
            // on the clip is visible through it as volume changes.
            // Closes when neither the icon nor this area is hovered.
            if let Some((vol_clip_id, anchor)) = self.vol_popup {
                // Look up this clip's current volume from state.
                let current_vol = state.timeline.iter()
                    .find(|c| c.id == vol_clip_id)
                    .map(|c| c.volume)
                    .unwrap_or(1.0);

                // Convert linear → dB for display and editing.
                let vol_to_db = |v: f32| -> f32 {
                    if v <= 0.0001 { -60.0 } else { 20.0 * v.log10() }
                };
                let db_to_vol = |db: f32| -> f32 { 10.0_f32.powf(db / 20.0) };

                let mut vol_db = vol_to_db(current_vol);

                // Position popup centered above the speaker icon, above the clip.
                let popup_w  = 64.0_f32;
                let popup_h  = 150.0_f32;
                let popup_pos = Pos2::new(
                    anchor.x - popup_w * 0.5,
                    anchor.y - popup_h - 14.0,
                );

                let area_resp = egui::Area::new(Id::new("vol_popup_area"))
                    .fixed_pos(popup_pos)
                    .order(egui::Order::Foreground)
                    .interactable(true)
                    .show(ui.ctx(), |ui| {
                        // Nearly-transparent dark frame — waveform shows through.
                        egui::Frame::new()
                            .fill(Color32::from_rgba_unmultiplied(18, 18, 24, 185))
                            .stroke(Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40)))
                            .corner_radius(egui::CornerRadius::same(6))
                            .inner_margin(egui::Margin::same(8))
                            .shadow(egui::Shadow {
                                offset: [0, 4],
                                blur: 10,
                                spread: 0,
                                color: Color32::from_black_alpha(100),
                            })
                            .show(ui, |ui| {
                                // Fix the inner width so the popup never resizes as the dB
                                // label text changes length (which causes the box to jitter).
                                let inner_w = popup_w - 16.0; // 16 = inner_margin * 2
                                ui.set_min_width(inner_w);
                                ui.set_max_width(inner_w);

                                // dB readout at top — rendered in a fixed-size slot so its
                                // varying text width can't push the container around.
                                let db_label = if vol_db <= -59.0 {
                                    "-∞ dB".to_string()
                                } else {
                                    format!("{:+.1} dB", vol_db)
                                };
                                ui.allocate_ui(Vec2::new(inner_w, 13.0), |ui| {
                                    ui.centered_and_justified(|ui| {
                                        ui.label(
                                            RichText::new(&db_label)
                                                .size(9.0)
                                                .monospace()
                                                .color(ACCENT),
                                        );
                                    });
                                });
                                ui.add_space(4.0);

                                // Vertical slider in dB space.
                                // Nearly transparent bg so waveform bleeds through.
                                let slider = egui::Slider::new(&mut vol_db, -60.0_f32..=6.0)
                                    .vertical()
                                    .show_value(false)
                                    .step_by(0.1);
                                let slider_resp = ui.add_sized([22.0, 110.0], slider);
                                if slider_resp.changed() {
                                    let new_vol = db_to_vol(vol_db).clamp(0.0, 2.0);
                                    cmd.push(EditorCommand::SetClipVolume {
                                        id: vol_clip_id, volume: new_vol,
                                    });
                                }

                                // 0 dB reference tick labels
                                ui.add_space(2.0);
                                ui.label(
                                    RichText::new("0 dB")
                                        .size(8.0)
                                        .color(Color32::from_rgba_unmultiplied(180, 180, 180, 120)),
                                );
                            });
                    });

                // Close on click outside — same guard as transition_popup.
                if !self.vol_popup_just_opened {
                    let clicked = ui.input(|i| i.pointer.any_click());
                    if clicked {
                        if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                            if !area_resp.response.rect.contains(pos) {
                                self.vol_popup = None;
                            }
                        }
                    }
                }
                self.vol_popup_just_opened = false;
            } else {
                self.vol_popup_just_opened = false;
            }
        });
    }
}

fn draw_waveform(painter: &egui::Painter, clip_rect: Rect, peaks: &[f32], clip_type: ClipType, volume: f32) {
    if peaks.is_empty() { return; }
    let w    = clip_rect.width();
    let h    = clip_rect.height();
    let mid_y = clip_rect.min.y + h * 0.5;
    let visible = (w as usize).min(peaks.len()).max(1);
    let step    = peaks.len() as f32 / visible as f32;
    let wave_color = if clip_type == ClipType::Audio {
        Color32::from_rgba_unmultiplied(100, 240, 165, 220)  // vivid green, high alpha — waveform is the whole point
    } else {
        Color32::from_rgba_unmultiplied(160, 200, 255, 38)   // very faint blue hint — thumbnails should read first
    };
    for i in 0..visible {
        let idx  = ((i as f32 * step) as usize).min(peaks.len() - 1);
        let peak = peaks[idx];
        let half = peak * volume * (h * 0.44);
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
/// Truncates `text` to fit within `max_px` using a per-character width
/// heuristic (11px proportional ≈ 6.5 px/char average). Appends "…" when
/// truncated. Avoids egui font measurement, which requires `&mut Fonts`.
fn fit_label(text: &str, max_px: f32) -> String {
    const AVG_CHAR_PX: f32 = 6.5;
    const ELLIPSIS: &str = "…";
    let max_chars = (max_px / AVG_CHAR_PX).max(0.0) as usize;
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    // Reserve one slot for the ellipsis character itself.
    let keep = max_chars.saturating_sub(1);
    text.chars().take(keep).collect::<String>() + ELLIPSIS
}