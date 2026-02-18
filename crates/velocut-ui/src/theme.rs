// src/theme.rs
use egui::{Context, Color32, Stroke, Visuals, Style};

// ── Palette ──────────────────────────────────────────────────────────────────
pub const ACCENT:        Color32 = Color32::from_rgb(255, 160,  50);
pub const ACCENT_DIM:    Color32 = Color32::from_rgb(180, 100,  20);
pub const ACCENT_HOVER:  Color32 = Color32::from_rgb(255, 185,  90);

pub const DARK_BG_0:     Color32 = Color32::from_rgb( 14,  14,  16);
pub const DARK_BG_1:     Color32 = Color32::from_rgb( 20,  20,  24);
pub const DARK_BG_2:     Color32 = Color32::from_rgb( 28,  28,  34);
pub const DARK_BG_3:     Color32 = Color32::from_rgb( 38,  38,  46);
pub const DARK_BG_4:     Color32 = Color32::from_rgb( 50,  50,  60);

pub const DARK_TEXT:     Color32 = Color32::from_rgb(220, 220, 230);
pub const DARK_TEXT_DIM: Color32 = Color32::from_rgb(120, 120, 138);
pub const DARK_BORDER:   Color32 = Color32::from_rgb( 55,  55,  68);

pub const CLIP_VIDEO:    Color32 = Color32::from_rgb( 52,  98, 168);
pub const CLIP_AUDIO:    Color32 = Color32::from_rgb( 42, 138,  98);
pub const CLIP_SELECTED: Color32 = Color32::from_rgb(200,  80,  50);

pub fn configure_style(ctx: &Context) {
    let mut style = Style::default();

    style.spacing.item_spacing     = egui::vec2(6.0, 5.0);
    style.spacing.window_margin    = egui::Margin::same(10);
    style.spacing.button_padding   = egui::vec2(10.0, 5.0);
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.indent           = 12.0;

    let cr = egui::CornerRadius::same(4);

    let mut v = Visuals::dark();
    v.panel_fill             = DARK_BG_1;
    v.window_fill            = DARK_BG_2;
    v.faint_bg_color         = DARK_BG_0;
    v.extreme_bg_color       = DARK_BG_0;
    v.window_stroke          = Stroke::new(1.0, DARK_BORDER);

    v.selection.bg_fill      = ACCENT;
    v.selection.stroke       = Stroke::new(1.0, Color32::BLACK);
    v.hyperlink_color        = ACCENT_HOVER;

    v.widgets.noninteractive.bg_fill       = DARK_BG_2;
    v.widgets.noninteractive.bg_stroke     = Stroke::new(1.0, DARK_BORDER);
    v.widgets.noninteractive.fg_stroke     = Stroke::new(1.0, DARK_TEXT_DIM);
    v.widgets.noninteractive.corner_radius = cr;

    v.widgets.inactive.bg_fill             = DARK_BG_3;
    v.widgets.inactive.bg_stroke           = Stroke::new(1.0, DARK_BORDER);
    v.widgets.inactive.fg_stroke           = Stroke::new(1.0, DARK_TEXT);
    v.widgets.inactive.corner_radius       = cr;

    v.widgets.hovered.bg_fill              = DARK_BG_4;
    v.widgets.hovered.bg_stroke            = Stroke::new(1.0, ACCENT_DIM);
    v.widgets.hovered.fg_stroke            = Stroke::new(1.5, ACCENT_HOVER);
    v.widgets.hovered.corner_radius        = cr;

    v.widgets.active.bg_fill               = ACCENT_DIM;
    v.widgets.active.bg_stroke             = Stroke::new(1.0, ACCENT);
    v.widgets.active.fg_stroke             = Stroke::new(2.0, Color32::WHITE);
    v.widgets.active.corner_radius         = cr;

    v.widgets.open.bg_fill                 = DARK_BG_4;
    v.widgets.open.bg_stroke               = Stroke::new(1.0, ACCENT_DIM);
    v.widgets.open.fg_stroke               = Stroke::new(1.5, ACCENT_HOVER);
    v.widgets.open.corner_radius           = cr;

    v.override_text_color = Some(DARK_TEXT);

    ctx.set_visuals(v);
    ctx.set_style(style);

    ctx.style_mut(|s| {
        s.visuals.window_corner_radius = cr;
        s.visuals.menu_corner_radius   = cr;
    });
}