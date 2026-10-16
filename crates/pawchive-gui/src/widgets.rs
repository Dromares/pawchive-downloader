//! Small drawing helpers shared by the panels: stock egui widgets, tokens from `theme`.

use eframe::egui::{
    self, vec2, Align, Button, Color32, CornerRadius, Frame, Layout, Margin, ProgressBar, Rect, Response, RichText,
    Sense, Stroke, Ui, UiBuilder,
};

use crate::theme::*;

pub fn dim(p: &Pal, s: impl Into<String>) -> RichText {
    RichText::new(s.into()).color(p.ink_dim).size(T_SMALL)
}

pub fn num(s: impl Into<String>) -> RichText {
    RichText::new(s.into()).monospace()
}

/// The one primary button (accent fill). Scoped so only this button's visuals change.
pub fn primary(ui: &mut Ui, p: &Pal, label: &str, w: f32, enabled: bool) -> Response {
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        for (ws, fill) in [
            (&mut v.widgets.inactive, p.accent),
            (&mut v.widgets.hovered, p.accent_hover),
            (&mut v.widgets.active, p.accent_hover),
        ] {
            ws.weak_bg_fill = fill;
            ws.bg_fill = fill;
            ws.fg_stroke = Stroke::new(1.0, p.on_accent);
            ws.bg_stroke = Stroke::NONE;
        }
        ui.add_enabled(enabled, Button::new(RichText::new(label).strong()).min_size(vec2(w, BTN_H)))
    })
    .inner
}

pub fn secondary(ui: &mut Ui, label: &str, w: f32, enabled: bool) -> Response {
    ui.add_enabled(enabled, Button::new(label).min_size(vec2(w, BTN_H)))
}

/// Card: surface fill, hairline, radius 8, a status stripe on the left edge.
pub fn card<R>(ui: &mut Ui, p: &Pal, stripe: Color32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let r = Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(CornerRadius::same(RADIUS + 2))
        .inner_margin(Margin { left: 18, right: 16, top: 14, bottom: 14 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(CARD_H);
            add(ui)
        });
    let rc = r.response.rect;
    let s = Rect::from_min_max(rc.left_top() + vec2(1.0, 6.0), egui::pos2(rc.left() + 5.0, rc.bottom() - 6.0));
    ui.painter().rect_filled(s, CornerRadius::same(2), stripe);
    r.inner
}

/// 8 pt determinate bar on a `sunken` track. No text inside, so contrast never depends on the fill.
pub fn bar(ui: &mut Ui, frac: f32, fill: Color32) {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 8.0;
        ui.add(
            ProgressBar::new(frac.clamp(0.0, 1.0))
                .desired_width(ui.available_width())
                .fill(fill)
                .corner_radius(CornerRadius::same(4)),
        );
    });
}

/// Indeterminate bar: a 28 % segment sliding on a sunken track. `t` runs 0..=1 and back.
pub fn indeterminate(ui: &mut Ui, p: &Pal, t: f32) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::ProgressIndicator, true, "Scanning, indeterminate"));
    ui.painter().rect_filled(rect, CornerRadius::same(4), p.sunken);
    let w = rect.width() * 0.28;
    let x = rect.left() + (rect.width() - w) * t;
    ui.painter().rect_filled(
        Rect::from_min_size(egui::pos2(x, rect.top()), vec2(w, 8.0)),
        CornerRadius::same(4),
        p.accent,
    );
}

/// Numbered step heading: a ring with the number, then the title.
pub fn heading(ui: &mut Ui, p: &Pal, n: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = S2;
        let (r, resp) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, format!("Step {n}")));
        ui.painter().circle_stroke(r.center(), 9.5, Stroke::new(1.0, p.line_strong));
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, n, egui::FontId::proportional(12.0), p.ink_dim);
        ui.label(RichText::new(title).strong().size(15.0));
    });
}

/// A sunken, read-only box with one truncated line (paths, "No folder chosen").
pub fn readout(ui: &mut Ui, p: &Pal, width: f32, text: RichText, hover: Option<&str>) {
    let r = Frame::new()
        .fill(p.sunken)
        .stroke(Stroke::new(1.0, p.line_strong))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(8, 7))
        .show(ui, |ui| {
            ui.set_width((width - 18.0).max(40.0));
            ui.add(egui::Label::new(text).truncate());
        });
    if let Some(h) = hover {
        r.response.on_hover_text(h);
    }
}

/// Fixed-height row split into cells; every cell is a child `Ui` in an exact rect so rows are the
/// same height whatever their content (target stability across states).
pub struct Cols {
    rect: Rect,
    widths: Vec<f32>,
}

impl Cols {
    pub fn new(ui: &mut Ui, height: f32, widths: Vec<f32>) -> Self {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
        Cols { rect, widths }
    }

    pub fn at(rect: Rect, widths: Vec<f32>) -> Self {
        Cols { rect, widths }
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn put(&self, ui: &mut Ui, i: usize, right: bool, f: impl FnOnce(&mut Ui)) {
        let x = self.rect.left() + self.widths[..i].iter().sum::<f32>();
        let rc = Rect::from_min_size(egui::pos2(x, self.rect.top()), vec2(self.widths[i], self.rect.height()));
        let lay = if right { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
        ui.scope_builder(UiBuilder::new().max_rect(rc.shrink2(vec2(4.0, 0.0))).layout(lay), f);
    }
}
