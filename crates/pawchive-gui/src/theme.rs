//! Token set and egui style from `ui-spec` (DAN-1542) section 3. Stock `Visuals`/`Style` fields only.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Theme,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Dark,
    Light,
}

impl From<Theme> for Mode {
    fn from(t: Theme) -> Self {
        match t {
            Theme::Dark => Mode::Dark,
            Theme::Light => Mode::Light,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Pal {
    pub mode: Mode,
    pub bg: Color32,
    pub surface: Color32,
    pub sunken: Color32,
    pub raised: Color32,
    pub hover: Color32,
    pub press: Color32,
    pub line: Color32,
    pub line_strong: Color32,
    pub ink: Color32,
    pub ink_dim: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub on_accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub bad: Color32,
}

fn rgb(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

impl Pal {
    pub fn new(mode: Mode) -> Self {
        match mode {
            Mode::Dark => Pal {
                mode,
                bg: rgb(0x15181E),
                surface: rgb(0x1C2028),
                sunken: rgb(0x101318),
                raised: rgb(0x272C37),
                hover: rgb(0x323947),
                press: rgb(0x3D4659),
                line: rgb(0x2C323D),
                line_strong: rgb(0x6F788D),
                ink: rgb(0xE8EBF1),
                ink_dim: rgb(0xA3ACBD),
                accent: rgb(0x6AA3FF),
                accent_hover: rgb(0x8DBAFF),
                on_accent: rgb(0x0A1428),
                ok: rgb(0x62C78F),
                warn: rgb(0xE8B84F),
                bad: rgb(0xF2767F),
            },
            Mode::Light => Pal {
                mode,
                bg: rgb(0xF3F4F7),
                surface: rgb(0xFFFFFF),
                sunken: rgb(0xE8EBF1),
                raised: rgb(0xE3E7EE),
                hover: rgb(0xD7DDE8),
                press: rgb(0xC9D1DF),
                line: rgb(0xD5DAE3),
                line_strong: rgb(0x78829A),
                ink: rgb(0x181C24),
                ink_dim: rgb(0x535E71),
                accent: rgb(0x1D5FD0),
                accent_hover: rgb(0x174FB0),
                on_accent: rgb(0xFFFFFF),
                ok: rgb(0x176B3E),
                warn: rgb(0x7D5000),
                bad: rgb(0xAE2A33),
            },
        }
    }

    fn selection_fill(&self) -> Color32 {
        match self.mode {
            Mode::Dark => rgb(0x2A4A7F),
            Mode::Light => rgb(0xB9D0F5),
        }
    }

    pub fn backdrop(&self) -> Color32 {
        Color32::from_black_alpha(if self.mode == Mode::Dark { 150 } else { 90 })
    }
}

// Type scale (points).
pub const T_TITLE: f32 = 20.0;
pub const T_BODY: f32 = 14.0;
pub const T_SMALL: f32 = 12.5;
pub const T_MONO: f32 = 13.0;
pub const T_CARD: f32 = 16.0;

// Spacing scale (points).
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
pub const S4: f32 = 16.0;
pub const S5: f32 = 24.0;

pub const ROW_H: f32 = 28.0;
pub const BTN_H: f32 = 36.0;
pub const LEFT_W: f32 = 400.0;
pub const CARD_H: f32 = 92.0;
pub const RADIUS: u8 = 6;

/// Where a system CJK face usually lives. Noto Sans CJK first, Droid Sans Fallback last.
const CJK_FILES: [&str; 6] = [
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/OTF/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
];

/// Append a system CJK face behind the stock fonts so Japanese post titles and file names do not
/// render as boxes. A missing file only costs the glyphs on screen; files on disk are named right.
pub fn install_fonts(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    if let Some(bytes) = CJK_FILES.iter().find_map(|p| std::fs::read(p).ok()) {
        defs.font_data.insert("cjk".into(), Arc::new(FontData::from_owned(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            defs.families.entry(family).or_default().push("cjk".into());
        }
    }
    ctx.set_fonts(defs);
}

/// Style both themes from their palettes; egui picks the active one from the system preference.
pub fn install_styles(ctx: &egui::Context) {
    for theme in [Theme::Dark, Theme::Light] {
        let p = Pal::new(theme.into());
        ctx.set_visuals_of(theme, if theme == Theme::Dark { egui::Visuals::dark() } else { egui::Visuals::light() });
        ctx.style_mut_of(theme, |s| apply(s, &p));
    }
}

fn apply(s: &mut egui::Style, p: &Pal) {
    let v = &mut s.visuals;
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.sunken;
    v.faint_bg_color = p.surface;
    v.window_stroke = Stroke::new(1.0, p.line_strong);
    v.window_corner_radius = CornerRadius::same(RADIUS + 2);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.window_shadow =
        egui::epaint::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(90) };
    v.selection.bg_fill = p.selection_fill();
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.text_cursor.stroke = Stroke::new(1.5, p.ink);
    let r = CornerRadius::same(RADIUS);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.ink);
    w.noninteractive.corner_radius = r;
    for (ws, fill, border) in [
        (&mut w.inactive, p.raised, Stroke::new(1.0, p.line_strong)),
        (&mut w.hovered, p.hover, Stroke::new(1.0, p.ink_dim)),
        (&mut w.active, p.press, Stroke::new(1.0, p.accent)),
        (&mut w.open, p.raised, Stroke::new(1.0, p.accent)),
    ] {
        ws.bg_fill = fill;
        ws.weak_bg_fill = fill;
        ws.bg_stroke = border;
        ws.fg_stroke = Stroke::new(1.0, p.ink);
        ws.corner_radius = r;
    }
    v.slider_trailing_fill = true;
    v.striped = false;
    s.spacing.item_spacing = egui::vec2(S2, S2);
    s.spacing.button_padding = egui::vec2(S3, 6.0);
    s.spacing.interact_size = egui::vec2(36.0, 30.0);
    s.spacing.window_margin = egui::Margin::same(S4 as i8);
    s.spacing.scroll = egui::style::ScrollStyle::solid();
    s.spacing.scroll.bar_width = 8.0;
    s.spacing.scroll.foreground_color = true;
    s.spacing.scroll.dormant_handle_opacity = 0.38;
    s.spacing.scroll.active_handle_opacity = 0.6;
    s.spacing.scroll.interact_handle_opacity = 0.9;
    s.spacing.slider_width = 200.0;
    s.spacing.icon_width = 18.0;
    s.spacing.icon_width_inner = 10.0;
    s.spacing.icon_spacing = S2;
    s.text_styles = [
        (TextStyle::Heading, FontId::new(T_TITLE, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(T_BODY, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(T_BODY, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(T_SMALL, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(T_MONO, FontFamily::Monospace)),
    ]
    .into();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lin(c: u8) -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    fn lum(c: Color32) -> f64 {
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }
    fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// The spec's measured-contrast table, recomputed from the palette we actually ship.
    #[test]
    fn palette_meets_the_spec_contrast_floors() {
        for mode in [Mode::Dark, Mode::Light] {
            let p = Pal::new(mode);
            for under in [p.bg, p.surface, p.sunken] {
                assert!(contrast(p.ink, under) >= 4.5, "{mode:?} ink");
                assert!(contrast(p.ink_dim, under) >= 4.5, "{mode:?} ink_dim");
                for c in [p.ok, p.warn, p.bad] {
                    assert!(contrast(c, under) >= 4.5, "{mode:?} signal text");
                }
            }
            assert!(contrast(p.ink_dim, p.raised) >= 4.5, "{mode:?} ink_dim on raised");
            assert!(contrast(p.on_accent, p.accent) >= 4.5, "{mode:?} primary label");
            assert!(contrast(p.on_accent, p.accent_hover) >= 4.5, "{mode:?} primary label hovered");
            assert!(contrast(p.accent, p.bg) >= 4.5, "{mode:?} accent");
            for under in [p.bg, p.surface] {
                assert!(contrast(p.line_strong, under) >= 3.0, "{mode:?} control border");
            }
            for fill in [p.accent, p.warn, p.bad, p.ok] {
                assert!(contrast(fill, p.sunken) >= 3.0, "{mode:?} bar fill on track");
            }
        }
    }
}
