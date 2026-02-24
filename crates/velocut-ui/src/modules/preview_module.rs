// crates/velocut-ui/src/modules/preview.rs
use super::EditorModule;
use velocut_core::state::{ProjectState, AspectRatio};
use velocut_core::commands::EditorCommand;
use velocut_core::helpers::time::format_time;
use crate::modules::ThumbnailCache;
use crate::theme::{ACCENT, DARK_BG_2, DARK_BG_3, DARK_BORDER};
use egui::{Ui, Color32, Sense, Rect, Pos2, Stroke, RichText, Vec2};

// ── Transport bar layout constants ───────────────────────────────────────────
const BAR_H:     f32 = 48.0;
const BTN_SIZE:  f32 = 30.0;   // every button is this exact square
const BTN_R:     f32 = 4.0;    // button corner radius
const ICON_SZ:   f32 = 9.0;    // half-size of painted icon geometry
const GAP:       f32 = 4.0;    // gap between buttons in the same group
const SEP:       f32 = 18.0;   // gap between groups
const VOL_W:     f32 = 80.0;   // volume slider width
// CONTENT_W = skip(30)+gap(4)+play(30)+gap(4)+stop(30) = 98
//           + sep(18) + timecode(66) + sep(18)         = 102
//           + mute(30)+gap(4)+vol(80)                  = 114
//           ──────────────────────────────────────────── 314
const CONTENT_W: f32 = 314.0;

// ── UV crop helper ────────────────────────────────────────────────────────────

/// Compute UV sample coordinates that center-crop a `tex_w × tex_h` texture to
/// `target_ar` (width / height).  Returns `(0,0)→(1,1)` when ARs already match.
///
/// egui's `painter.image()` UV rect controls which portion of the texture is
/// sampled — passing a sub-rect here is a zero-cost GPU crop with no scaling
/// artefacts, and exactly mirrors the center-crop applied in `encode.rs`.
fn crop_uv_rect(tex_w: f32, tex_h: f32, target_ar: f32) -> Rect {
    if tex_h < 1.0 || tex_w < 1.0 {
        return Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    }
    let src_ar = tex_w / tex_h;
    if (src_ar - target_ar).abs() < 0.01 {
        return Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    }
    if src_ar > target_ar {
        // Source wider — crop left and right.
        let visible_w = tex_h * target_ar;
        let u_margin  = (tex_w - visible_w) / (2.0 * tex_w);
        Rect::from_min_max(Pos2::new(u_margin, 0.0), Pos2::new(1.0 - u_margin, 1.0))
    } else {
        // Source taller — crop top and bottom.
        let visible_h = tex_w / target_ar;
        let v_margin  = (tex_h - visible_h) / (2.0 * tex_h);
        Rect::from_min_max(Pos2::new(0.0, v_margin), Pos2::new(1.0, 1.0 - v_margin))
    }
}

pub struct PreviewModule {
    /// The live decoded frame for the current playhead position, set by app.rs
    /// each frame before ui() is called. When Some, it takes priority over the
    /// thumbnail in thumb_cache — thumbnail_cache stays pure thumbnails.
    pub current_frame: Option<egui::TextureHandle>,
    /// Last successfully decoded frame. Held across ticks so that brief gaps
    /// during clip transitions and scrub decode latency never flash the thumbnail.
    /// Cleared only when the playhead moves to a region with no timeline clip.
    pub held_frame: Option<egui::TextureHandle>,
    /// Pixel size of the video canvas rect as rendered last frame.
    /// Written by ui() before tick_modules() runs so VideoModule::tick() can
    /// pass it to start_playback as forced_size — decoding at panel resolution
    /// instead of native 1080p cuts swscale CPU from ~8% to ~0.5% and drops
    /// channel memory from 48 MB to ~6 MB.
    pub last_canvas_size: Option<(u32, u32)>,
}

impl PreviewModule {
    pub fn new() -> Self { Self { current_frame: None, held_frame: None, last_canvas_size: None } }

    pub fn reset(&mut self) {
        self.current_frame = None;
        self.held_frame    = None;
        // Do not clear last_canvas_size — panel size persists across playback resets.
    }
}

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
                        ui.label(RichText::new("🖼 Monitor").size(12.0).strong());
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

            // ── Video Canvas ─────────────────────────────────────────────────
            // Full panel_w allocated, canvas drawn centered inside it — same
            // pattern that already works correctly.
            let ratio   = state.active_video_ratio();
            let panel_w = ui.available_width();
            let panel_h = (ui.available_height() - BAR_H - 12.0).max(80.0);

            let (canvas_w, canvas_h) = {
                let h = panel_w / ratio;
                if h <= panel_h { (panel_w, h) } else { (panel_h * ratio, panel_h) }
            };

            // Record canvas pixel size so VideoModule::tick() can pass it as
            // forced_size to LiveDecoder::open().  round to even (swscale req).
            // render_panels() runs before tick_modules() so this is always fresh.
            let cw = ((canvas_w as u32) & !1).max(2);
            let ch = ((canvas_h as u32) & !1).max(2);
            self.last_canvas_size = Some((cw, ch));

            let (outer_rect, _) = ui.allocate_exact_size(
                Vec2::new(panel_w, canvas_h), Sense::hover());
            let canvas = Rect::from_center_size(
                outer_rect.center(), Vec2::new(canvas_w, canvas_h));
            let painter = ui.painter();

            if state.is_playing {
                painter.rect_stroke(canvas.expand(2.0), 4.0,
                    Stroke::new(1.5, ACCENT.gamma_multiply(0.55)),
                    egui::StrokeKind::Outside);
            } else {
                painter.rect_stroke(canvas.expand(1.0), 4.0,
                    Stroke::new(1.0, DARK_BORDER),
                    egui::StrokeKind::Outside);
            }
            painter.rect_filled(canvas, 3.0, Color32::BLACK);

            let current_clip = state.timeline.iter().find(|c| {
                state.current_time >= c.start_time
                    && state.current_time < c.start_time + c.duration
            });

            if let Some(clip) = current_clip {
                if let Some(media) = state.library.iter().find(|m| m.id == clip.media_id) {
                    // Update held_frame whenever we have a fresh decoded frame.
                    // This persists across ticks so clip transitions and scrub latency
                    // never drop back to the thumbnail — we show the last good frame instead.
                    if self.current_frame.is_some() {
                        self.held_frame = self.current_frame.clone();
                    }
                    let canvas_tex = self.held_frame.as_ref()
                        .or_else(|| thumb_cache.get(&media.id));
                    if let Some(tex) = canvas_tex {
                        let [tw, th] = tex.size();
                        let uv = crop_uv_rect(tw as f32, th as f32, ratio);
                        painter.image(tex.id(), canvas, uv, Color32::WHITE);
                    } else {
                        // Thumbnail not yet loaded — name + spinner
                        painter.text(
                            canvas.center() - egui::vec2(0.0, 20.0),
                            egui::Align2::CENTER_CENTER,
                            &media.name,
                            egui::FontId::proportional(13.0),
                            Color32::from_gray(70));
                        let t  = ui.input(|i| i.time) as f32;
                        let cx = canvas.center() + egui::vec2(0.0, 20.0);
                        let r  = 12.0_f32;
                        painter.circle_stroke(cx, r, Stroke::new(1.5, Color32::from_gray(35)));
                        let a  = t * 3.5;
                        painter.line_segment(
                            [cx, cx + egui::vec2(a.cos() * r, a.sin() * r)],
                            Stroke::new(2.0, ACCENT));
                        ui.ctx().request_repaint();
                    }
                }
            } else {
                // No clip under playhead — clear held_frame so we don't show a
                // stale frame when the user scrubs to an empty region of the timeline.
                self.held_frame = None;
                painter.text(canvas.center(), egui::Align2::CENTER_CENTER,
                    "NO SIGNAL", egui::FontId::monospace(14.0), Color32::from_gray(40));
                let mut y = canvas.min.y;
                while y < canvas.max.y {
                    painter.line_segment(
                        [Pos2::new(canvas.min.x, y), Pos2::new(canvas.max.x, y)],
                        Stroke::new(0.5, Color32::from_rgba_unmultiplied(255, 255, 255, 3)));
                    y += 4.0;
                }
            }

            ui.add_space(6.0);

            // ── Transport Bar ─────────────────────────────────────────────────
            let bar_w = ui.available_width();
            let (bar_rect, _) = ui.allocate_exact_size(
                Vec2::new(bar_w, BAR_H), Sense::hover());

            let painter = ui.painter();
            painter.rect_filled(bar_rect, BTN_R, DARK_BG_3);
            painter.rect_stroke(bar_rect, BTN_R,
                Stroke::new(1.0, DARK_BORDER), egui::StrokeKind::Outside);

            let cy = bar_rect.center().y;
            let show_volume  = bar_w >= CONTENT_W + 16.0;
            let content_w    = if show_volume { CONTENT_W } else { CONTENT_W - GAP - VOL_W };
            let mut x = (bar_rect.center().x - content_w / 2.0)
                .max(bar_rect.min.x + 6.0);

            macro_rules! tbtn {
                ($id:expr, $active:expr, $draw_icon:expr) => {{
                    let r = Rect::from_min_size(
                        Pos2::new(x, cy - BTN_SIZE / 2.0),
                        Vec2::splat(BTN_SIZE));
                    let resp = ui.interact(r, ui.id().with($id), Sense::click());
                    let (bg, icol) = if resp.is_pointer_button_down_on() {
                        (DARK_BG_2.gamma_multiply(0.6), Color32::WHITE)
                    } else if resp.hovered() {
                        (DARK_BG_2, ACCENT.linear_multiply(1.2))
                    } else if $active {
                        (DARK_BG_3, ACCENT)
                    } else {
                        (DARK_BG_3, Color32::from_gray(175))
                    };
                    painter.rect_filled(r, BTN_R, bg);
                    if resp.hovered() || $active {
                        painter.rect_stroke(r, BTN_R,
                            Stroke::new(1.0, ACCENT.gamma_multiply(0.35)),
                            egui::StrokeKind::Outside);
                    }
                    let c = r.center();
                    $draw_icon(c, icol);
                    x += BTN_SIZE;
                    resp.clicked()
                }};
            }

            // ── Skip to Start ─────────────────────────────────────────────
            if tbtn!("skip_back", false, |c: Pos2, col: Color32| {
                painter.rect_filled(
                    Rect::from_center_size(
                        Pos2::new(c.x - ICON_SZ + 0.5, c.y),
                        Vec2::new(2.5, ICON_SZ * 2.0)),
                    0.5, col);
                painter.add(egui::Shape::convex_polygon(vec![
                    Pos2::new(c.x - ICON_SZ + 4.0, c.y),
                    Pos2::new(c.x + ICON_SZ - 1.0,  c.y - ICON_SZ + 1.0),
                    Pos2::new(c.x + ICON_SZ - 1.0,  c.y + ICON_SZ - 1.0),
                ], col, Stroke::NONE));
            }) {
                cmd.push(EditorCommand::Stop);
            }
            x += GAP;

            // ── Play / Pause ──────────────────────────────────────────────
            let playing = state.is_playing;
            if tbtn!("play_pause", playing, |c: Pos2, col: Color32| {
                if playing {
                    for ox in [-ICON_SZ * 0.45, ICON_SZ * 0.45] {
                        painter.rect_filled(
                            Rect::from_center_size(
                                Pos2::new(c.x + ox, c.y),
                                Vec2::new(3.0, ICON_SZ * 1.8)),
                            1.0, col);
                    }
                } else {
                    painter.add(egui::Shape::convex_polygon(vec![
                        Pos2::new(c.x - ICON_SZ * 0.5, c.y - ICON_SZ),
                        Pos2::new(c.x - ICON_SZ * 0.5, c.y + ICON_SZ),
                        Pos2::new(c.x + ICON_SZ,        c.y),
                    ], col, Stroke::NONE));
                }
            }) {
                if state.is_playing { cmd.push(EditorCommand::Pause); }
                else                { cmd.push(EditorCommand::Play);  }
            }
            x += GAP;

            // ── Stop ──────────────────────────────────────────────────────
            if tbtn!("stop", false, |c: Pos2, col: Color32| {
                painter.rect_filled(
                    Rect::from_center_size(c, Vec2::splat(ICON_SZ * 1.5)),
                    1.5, col);
            }) {
                cmd.push(EditorCommand::Stop);
            }
            x += SEP;

            // ── Timecode ──────────────────────────────────────────────────
            painter.text(
                Pos2::new(x, cy),
                egui::Align2::LEFT_CENTER,
                format_time(state.current_time),
                egui::FontId::monospace(12.0),
                ACCENT);
            x += 66.0 + SEP;

            // ── Mute ──────────────────────────────────────────────────────
            let muted   = state.muted;
            let vol_val = state.volume;
            if tbtn!("mute", muted, |c: Pos2, col: Color32| {
                painter.add(egui::Shape::convex_polygon(vec![
                    Pos2::new(c.x - ICON_SZ + 1.0, c.y - ICON_SZ * 0.4),
                    Pos2::new(c.x - ICON_SZ + 1.0, c.y + ICON_SZ * 0.4),
                    Pos2::new(c.x + 1.0,            c.y + ICON_SZ * 0.9),
                    Pos2::new(c.x + 1.0,            c.y - ICON_SZ * 0.9),
                ], col, Stroke::NONE));
                if !muted && vol_val > 0.0 {
                    painter.circle_stroke(
                        Pos2::new(c.x + 2.0, c.y), ICON_SZ * 0.85,
                        Stroke::new(1.5, col.gamma_multiply(0.65)));
                }
                if !muted && vol_val > 0.5 {
                    painter.circle_stroke(
                        Pos2::new(c.x + 2.0, c.y), ICON_SZ * 1.45,
                        Stroke::new(1.5, col.gamma_multiply(0.35)));
                }
                if muted {
                    let ox = c.x + ICON_SZ * 0.35;
                    let mute_col = Color32::from_rgb(200, 60, 60);
                    painter.line_segment(
                        [Pos2::new(ox - 4.0, c.y - 4.0), Pos2::new(ox + 4.0, c.y + 4.0)],
                        Stroke::new(1.5, mute_col));
                    painter.line_segment(
                        [Pos2::new(ox + 4.0, c.y - 4.0), Pos2::new(ox - 4.0, c.y + 4.0)],
                        Stroke::new(1.5, mute_col));
                }
            }) {
                cmd.push(EditorCommand::ToggleMute);
            }
            x += GAP;

            // ── Volume Slider ─────────────────────────────────────────────
            if show_volume {
                let vol_rect = Rect::from_min_size(
                    Pos2::new(x, cy - BTN_SIZE / 2.0),
                    Vec2::new(VOL_W, BTN_SIZE));
                let mut vol = state.volume;
                if ui.put(vol_rect,
                    egui::Slider::new(&mut vol, 0.0_f32..=1.0_f32)
                        .show_value(false)
                        .trailing_fill(true)
                ).changed() {
                    cmd.push(EditorCommand::SetVolume(vol));
                }
            }

        }); // ui.vertical
    }
}