//! The External links group (`ui-spec-external-links`): the row in step 2 and the centre tab with its
//! toolbar, host chips and the keyboard-driven list. Drawing only; ticks go straight to the model
//! (like the type boxes) and checks are queued in `App::link_checks`.

use std::time::Instant;

use eframe::egui::{
    self, pos2, vec2, Align, Button, Color32, CornerRadius, EventFilter, Id, Key, Layout, Modifiers, Rect, RichText,
    ScrollArea, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder,
};
use pawchive_core::external::ResolvedKind;
use pawchive_core::links::LinkKind;

use crate::app::{Action, App, ExportFormat};
use crate::format::{comma, human_size};
use crate::links_state::{self, Check, LinkFilter, LinkRow, Tri, Vis};
use crate::state::{explain_failure, Phase, Row, RowState};
use crate::theme::*;
use crate::view::size_label;
use crate::widgets::{dim, num, Cols};

const LROW_H: f32 = 44.0;

/// Status glyphs. The no-entry sign and the chevron are drawn: the bundled fonts have neither.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ic {
    Dot,
    Spin,
    Check,
    Cross,
    Down,
    Dots,
    NoEntry,
    Pause,
}

fn paint_icon(ui: &mut Ui, p: &Pal, ic: Ic, c: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
    let ctr = r.center();
    let font = egui::FontId::proportional(14.0);
    let text = |ui: &Ui, t: &str, f: egui::FontId| {
        ui.painter().text(ctr, egui::Align2::CENTER_CENTER, t, f, c);
    };
    match ic {
        Ic::Dot => text(ui, "•", font),
        Ic::Check => text(ui, "✓", font),
        Ic::Cross => text(ui, "×", egui::FontId::proportional(16.0)),
        Ic::Down => text(ui, "↓", font),
        Ic::Dots => text(ui, "…", font),
        Ic::NoEntry => {
            ui.painter().circle_stroke(ctr, 6.5, Stroke::new(1.5, c));
            ui.painter().line_segment([ctr + vec2(-4.6, 4.6), ctr + vec2(4.6, -4.6)], Stroke::new(1.5, c));
        }
        Ic::Pause => {
            for dx in [-3.0, 1.5] {
                ui.painter().rect_filled(
                    Rect::from_min_size(ctr + vec2(dx - 0.5, -5.0), vec2(3.0, 10.0)),
                    CornerRadius::same(1),
                    c,
                );
            }
        }
        Ic::Spin => {
            ui.put(r, egui::Spinner::new().size(14.0).color(p.accent));
        }
    }
}

/// Drawn disclosure triangle (`▸ ▾` render as boxes in the bundled font).
fn chevron(ui: &mut Ui, c: Color32, open: bool, at: egui::Pos2) {
    let pts = if open {
        vec![at + vec2(-4.5, -2.5), at + vec2(4.5, -2.5), at + vec2(0.0, 3.0)]
    } else {
        vec![at + vec2(-2.5, -4.5), at + vec2(-2.5, 4.5), at + vec2(3.0, 0.0)]
    };
    ui.painter().add(Shape::convex_polygon(pts, c, Stroke::NONE));
}

/// Line 1 of a folder child: `dir/` in `ink_dim`, then the leaf in `ink`. When it does not fit,
/// directory components are dropped from the left (the nearest parent last); the leaf is never elided
/// from the middle and is cut at its end only when it alone is wider than the cell.
fn path_label(ui: &mut Ui, p: &Pal, full: &str, avail: f32) {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let width = |ui: &Ui, t: &str, c: Color32| ui.painter().layout_no_wrap(t.to_string(), font.clone(), c).size().x;
    let (dir, leaf) = match full.rfind('/') {
        Some(i) => (&full[..=i], &full[i + 1..]),
        None => ("", full),
    };
    let leaf_w = width(ui, leaf, p.ink);
    let mut shown = dir.to_string();
    if !dir.is_empty() && width(ui, dir, p.ink_dim) + leaf_w > avail {
        let comps: Vec<&str> = dir.trim_end_matches('/').split('/').collect();
        shown = String::from("…/");
        for k in 1..comps.len() {
            let cand = format!("…/{}/", comps[k..].join("/"));
            if width(ui, &cand, p.ink_dim) + leaf_w <= avail {
                shown = cand;
                break;
            }
        }
    }
    ui.spacing_mut().item_spacing.x = 0.0;
    if !shown.is_empty() {
        ui.label(RichText::new(shown).color(p.ink_dim));
    }
    ui.add(egui::Label::new(leaf).truncate());
}

/// A list-row checkbox that looks like the stock one but is not a Tab stop: the whole list is one stop
/// (section 2.3), so 500 rows cost one Tab press.
fn row_check(ui: &mut Ui, tri: Tri, enabled: bool, label: &str) -> egui::Response {
    let size = ui.spacing().icon_width + 6.0;
    let sense = if enabled { Sense::CLICK } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), sense);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, enabled, tri == Tri::All, label));
    if ui.is_rect_visible(rect) {
        let v = ui.style().interact(&resp);
        let icon = Rect::from_center_size(rect.center(), vec2(ui.spacing().icon_width, ui.spacing().icon_width));
        let fade = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };
        ui.painter().rect(
            icon.expand(v.expansion),
            v.corner_radius,
            fade(v.bg_fill),
            Stroke::new(v.bg_stroke.width, fade(v.bg_stroke.color)),
            StrokeKind::Inside,
        );
        let st = Stroke::new(v.fg_stroke.width.max(1.5), fade(v.fg_stroke.color));
        let small = icon.shrink(icon.width() * 0.22);
        match tri {
            Tri::All => {
                ui.painter().add(Shape::line(
                    vec![
                        pos2(small.left(), small.center().y),
                        pos2(small.center().x - 0.5, small.bottom()),
                        pos2(small.right(), small.top()),
                    ],
                    st,
                ));
            }
            Tri::Some => {
                ui.painter()
                    .line_segment([pos2(small.left(), small.center().y), pos2(small.right(), small.center().y)], st);
            }
            Tri::None => {}
        }
    }
    resp
}

/// The Done / Failed word for a finished set of jobs.
struct Look {
    ic: Ic,
    color: Color32,
    word: String,
    line2: Option<String>,
    /// `ⓘ` in front of line 2 (a folder listing that may be incomplete).
    info: bool,
    bar: Option<(f32, Color32)>,
    tooltip: Option<String>,
}

impl Look {
    fn simple(ic: Ic, color: Color32, word: impl Into<String>, line2: Option<String>) -> Self {
        Look { ic, color, word: word.into(), line2, info: false, bar: None, tooltip: None }
    }
}

fn unavailable_text(msg: &str) -> &'static str {
    if msg.contains("timed out") {
        "Couldn't check (timed out)"
    } else {
        "Couldn't check"
    }
}

const INCOMPLETE_TIP: &str = "Google Drive didn't confirm that this list is complete. Only the {n} files shown will be downloaded. Open the folder in your browser to compare.";

impl App {
    /// Tick (or clear) every link that can be ticked, whatever the filter; unchecked ones get checked.
    pub(crate) fn tick_all_links(&mut self, on: bool) {
        for i in 0..self.model.links.len() {
            self.tick_link(i, on);
        }
    }

    fn tick_link(&mut self, i: usize, on: bool) {
        let r = &mut self.model.links[i];
        r.set_on(on);
        if on && r.needs_check() && r.can_tick() {
            self.link_checks.push(i);
        }
    }

    /// Select or clear the shown rows that can be ticked; rows the filter hides are never touched.
    fn tick_shown(&mut self, on: bool) {
        for i in links_state::shown(&self.model.links, self.model.link_filter) {
            self.tick_link(i, on);
        }
    }

    // ---- step 2: the External links row ------------------------------------------------------------

    pub(crate) fn left_links_row(&mut self, ui: &mut Ui, p: &Pal, editable: bool, live: bool) {
        ui.add_space(S2);
        let t = links_state::totals(&self.model.links);
        let tri = Tri::of(t.selected, t.found);
        let w = ui.available_width();
        let mut on = tri == Tri::All;
        let mut changed = false;
        let c = Cols::new(ui, ROW_H + 2.0, vec![w - 40.0 - 84.0, 40.0, 84.0]);
        c.put(ui, 0, false, |ui| {
            ui.add_enabled_ui(editable && t.found > 0, |ui| {
                let cb = egui::Checkbox::new(&mut on, "External links").indeterminate(tri == Tri::Some);
                let r = ui.add(cb).on_hover_text("All external links, whatever the filter");
                changed = r.changed();
            });
        });
        c.put(ui, 1, true, |ui| {
            let s = if live { "…".into() } else { comma(t.found) };
            ui.label(num(s).color(if t.found > 0 || live { p.ink } else { p.ink_dim }));
        });
        c.put(ui, 2, true, |ui| {
            if live || t.found == 0 || (t.found_unknown && t.found_bytes == 0) {
                ui.label(num(if live { "…" } else { "—" }).color(p.ink_dim));
            } else if t.found_unknown {
                ui.label(num(size_label(t.found_bytes, true)).color(p.ink_dim));
            } else {
                ui.label(num(human_size(t.found_bytes)));
            }
        });
        if changed {
            // None or some selected: take every downloadable link file. All selected: clear.
            self.tick_all_links(tri != Tri::All);
        }
        egui::Frame::new().inner_margin(egui::Margin { left: 26, right: 0, top: 0, bottom: 0 }).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let any_supported = self.model.links.iter().any(|r| r.link.downloadable());
            let note = if self.model.links.is_empty() {
                "No Google Drive or Mediafire links in these posts."
            } else if !any_supported {
                "None of these links can be downloaded. They are still listed in the External links tab."
            } else {
                "Third-party downloads. Choose them link by link in the External links tab; the type boxes above don't apply."
            };
            ui.add(egui::Label::new(dim(p, note)).wrap());
        });
    }

    // ---- the tab ---------------------------------------------------------------------------------------

    pub(crate) fn links_tab(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        let w = ui.available_width();
        let now = Instant::now();
        let editable = self.model.phase == Phase::Ready;
        let n = self.model.links.len();
        let filter = self.model.link_filter;
        let narrow = w < 600.0;
        let shown = links_state::shown(&self.model.links, filter);
        let sel_shown = links_state::totals(shown.iter().map(|&i| &self.model.links[i])).selected;
        let found_shown = links_state::totals(shown.iter().map(|&i| &self.model.links[i])).found;
        let (check_n, check_min) =
            links_state::check_scope(&self.model.links, filter, self.settings.delay_ms as f64 / 1000.0);
        let cooling = [pawchive_core::external::ExtHost::Drive, pawchive_core::external::ExtHost::Mediafire]
            .into_iter()
            .find_map(|h| self.model.host_cooldown_secs(h, now).map(|s| (h, s)));

        // ---- toolbar row A (30 pt): notice / cool-down / progress / summary, then the two buttons
        ui.horizontal(|ui| {
            ui.set_min_height(30.0);
            ui.spacing_mut().item_spacing.x = S2;
            let left_w = (ui.available_width() - 210.0).max(100.0);
            ui.allocate_ui_with_layout(vec2(left_w, 30.0), Layout::left_to_right(Align::Center), |ui| {
                if let Some((lvl, text)) = &self.link_notice {
                    let (g, c) = match lvl {
                        crate::state::Level::Info => ("✓", p.ok),
                        crate::state::Level::Warn => ("⚠", p.warn),
                        crate::state::Level::Error => ("×", p.bad),
                    };
                    ui.label(RichText::new(g).color(c));
                    ui.add(egui::Label::new(RichText::new(text).size(T_SMALL)).truncate());
                } else if let Some((host, secs)) = cooling {
                    ui.label(RichText::new("⚠").color(p.warn));
                    ui.add(
                        egui::Label::new(RichText::new(format!(
                            "{} asked us to slow down · resuming in {secs} s",
                            host.name()
                        ))
                        .size(T_SMALL)
                        .color(p.warn))
                        .truncate(),
                    );
                } else if let Some((done, total)) = self.model.checking {
                    ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                    ui.add(egui::Label::new(dim(p, format!("Checking links · {done} of {total}"))).truncate());
                    if ui.add(Button::new(RichText::new("Skip").size(T_SMALL)).small()).clicked() {
                        acts.push(Action::SkipChecking);
                    }
                } else {
                    let text = links_state::summary_line(
                        &self.model.links,
                        filter,
                        self.model.scan_post.is_some(),
                        self.model.links_posts,
                        narrow,
                    );
                    ui.add(egui::Label::new(dim(p, text)).wrap());
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = S2;
                let can_export = n > 0 && !matches!(self.model.phase, Phase::Scanning | Phase::Empty | Phase::ScanError);
                ui.add_enabled_ui(can_export, |ui| {
                    let r = ui.menu_button("Export list    ", |ui| {
                        if ui.button("As text (.txt)").clicked() {
                            acts.push(Action::ExportList(ExportFormat::Txt));
                            ui.close();
                        }
                        if ui.button("As CSV (.csv)").clicked() {
                            acts.push(Action::ExportList(ExportFormat::Csv));
                            ui.close();
                        }
                    });
                    let rc = r.response.rect;
                    chevron(ui, if can_export { p.ink } else { p.ink_dim }, true, pos2(rc.right() - 14.0, rc.center().y));
                });
                let can_check = editable && check_n > 0 && self.model.checking.is_none();
                let tip = format!(
                    "Checks the {check_n} links shown that haven't been checked yet. About {} min at the current pause.",
                    check_min.max(1)
                );
                let r = ui.add_enabled(can_check, Button::new("Check links"));
                if r.on_hover_text(tip).clicked() {
                    acts.push(Action::CheckLinks);
                }
            });
        });

        // ---- toolbar row B (28 pt): the host chips, a fixed set in a fixed order
        if n > 0 {
            let counts = links_state::chip_counts(&self.model.links);
            ui.horizontal(|ui| {
                ui.set_min_height(28.0);
                ui.spacing_mut().item_spacing.x = S1;
                for (f, c) in LinkFilter::ALL.into_iter().zip(counts) {
                    let mut t = RichText::new(format!("{} {}", f.label(), comma(c))).size(T_SMALL);
                    if c == 0 && f != self.model.link_filter {
                        t = t.color(p.ink_dim);
                    }
                    ui.selectable_value(&mut self.model.link_filter, f, t);
                }
            });
        }
        ui.add_space(S1);
        if n == 0 {
            ui.add_space(S5);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No external links").strong());
                ui.label(dim(
                    p,
                    if self.model.scan_post.is_some() {
                        "This post has no links to Google Drive, Mediafire or other sites."
                    } else {
                        "Links to Google Drive, Mediafire and other sites found in post text are listed here."
                    },
                ));
            });
            return;
        }
        let filter = self.model.link_filter;

        // ---- columns
        let (c_cb, c_stat, c_size) = (32.0, if narrow { 142.0 } else { 184.0 }, if narrow { 88.0 } else { 100.0 });
        let c_text = (w - c_cb - c_stat - c_size - 14.0).max(120.0);

        // ---- header (28 pt, sunken). Its checkbox acts on the shown rows only.
        {
            let (rect, _) = ui.allocate_exact_size(vec2(w, 28.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius { nw: RADIUS, ne: RADIUS, sw: 0, se: 0 }, p.sunken);
            let tri = Tri::of(sel_shown, found_shown);
            let mut on = tri == Tri::All;
            let lt = Layout::left_to_right(Align::Center);
            let at = |x: f32, wd: f32| Rect::from_min_size(pos2(rect.left() + x, rect.top()), vec2(wd, rect.height()));
            let mut changed = false;
            ui.scope_builder(UiBuilder::new().max_rect(at(8.0, c_cb)).layout(lt), |ui| {
                ui.add_enabled_ui(editable && found_shown > 0, |ui| {
                    let r = ui.add(egui::Checkbox::without_text(&mut on).indeterminate(tri == Tri::Some));
                    changed = r.changed();
                    let tip = if tri == Tri::All {
                        format!("Clear the {} shown links", comma(shown.len()))
                    } else {
                        format!("Select all {} shown links", comma(shown.len()))
                    };
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::CheckBox, true, "Select all shown links"));
                    r.on_hover_text(tip);
                });
            });
            ui.scope_builder(UiBuilder::new().max_rect(at(c_cb + 4.0, c_text)).layout(lt), |ui| {
                ui.label(dim(p, "Link  ·  host  ·  source post"));
            });
            ui.scope_builder(UiBuilder::new().max_rect(at(c_cb + c_text, c_stat)).layout(lt), |ui| {
                ui.label(dim(p, "Status"));
            });
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(at(c_cb + c_text + c_stat, c_size))
                    .layout(Layout::right_to_left(Align::Center)),
                |ui| {
                    ui.label(dim(p, "Size"));
                },
            );
            if changed {
                self.tick_shown(tri != Tri::All);
            }
        }

        // ---- the list: ONE Tab stop with a roving focus row (section 2.3)
        let vis = links_state::visible(&self.model.links, filter);
        let avail = ui.available_height();
        let list_id = Id::unique("external_links_list");
        let top_left = ui.cursor().min;
        let list_rect = Rect::from_min_size(top_left, vec2(w, (avail - 4.0).max(60.0)));
        ui.interact(list_rect, list_id, Sense::focusable_noninteractive());
        let focused = ui.memory(|m| m.has_focus(list_id));
        let mut want_offset: Option<f32> = None;
        self.link_focus = self.link_focus.min(vis.len().saturating_sub(1));
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    list_id,
                    EventFilter { horizontal_arrows: true, vertical_arrows: true, tab: false, escape: false },
                );
            });
            want_offset = self.list_keys(ui, &vis, editable);
        }

        let mut toggles: Vec<(Vis, bool)> = Vec::new();
        let mut expands: Vec<usize> = Vec::new();
        let mut focus_to: Option<usize> = None;
        let focus_row = self.link_focus;
        egui::Frame::new()
            .stroke(Stroke::new(1.0, p.line))
            .corner_radius(CornerRadius { nw: 0, ne: 0, sw: RADIUS, se: RADIUS })
            .show(ui, |ui| {
                ui.set_width(w - 2.0);
                ui.set_min_height((avail - 4.0).max(0.0));
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut sa = ScrollArea::vertical().auto_shrink([false, false]).max_height((avail - 4.0).max(60.0));
                if let Some(o) = want_offset {
                    sa = sa.vertical_scroll_offset(o);
                }
                let out =
                    sa.show_rows(ui, LROW_H, vis.len(), |ui, range| {
                        for r in range {
                            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), LROW_H), Sense::hover());
                            let v = vis[r];
                            let kid = matches!(v, Vis::Kid(..));
                            if kid {
                                ui.painter().rect_filled(rect, 0.0, p.sunken);
                            } else if r % 2 == 1 {
                                ui.painter().rect_filled(rect, 0.0, p.surface);
                            }
                            if focused && focus_row == r {
                                ui.painter().rect_stroke(
                                    rect.shrink(1.0),
                                    2.0,
                                    Stroke::new(2.0, p.accent),
                                    StrokeKind::Inside,
                                );
                            }
                            self.link_row(ui, p, rect, r, v, (c_cb, c_text, c_stat, c_size), editable, now, &mut |a| {
                                match a {
                                    RowAct::Toggle(v, on) => toggles.push((v, on)),
                                    RowAct::Expand(i) => expands.push(i),
                                    RowAct::Focus(r) => focus_to = Some(r),
                                }
                            });
                        }
                    });
                self.link_view = (out.state.clamped_offset().y, out.inner_rect.height());
            });
        if let Some(r) = focus_to {
            self.link_focus = r;
            ui.memory_mut(|m| m.request_focus(list_id));
        }
        for (v, on) in toggles {
            match v {
                Vis::Link(i) => self.tick_link(i, on),
                Vis::Kid(i, j) => self.model.links[i].set_kid(j, on),
            }
        }
        for i in expands {
            let r = &mut self.model.links[i];
            r.open = !r.open;
        }
    }

    /// Keyboard model of the list. Returns the scroll offset to apply when the focus row moved.
    fn list_keys(&mut self, ui: &mut Ui, vis: &[Vis], editable: bool) -> Option<f32> {
        if vis.is_empty() {
            return None;
        }
        let ctx = ui.ctx().clone();
        let key = |k: Key, m: Modifiers| ctx.input_mut(|i| i.consume_key(m, k));
        let (offset, view_h) = self.link_view;
        let page = ((view_h / LROW_H).floor() as usize).max(1);
        let last = vis.len() - 1;
        let mut f = self.link_focus.min(last);
        let before = f;
        if key(Key::ArrowDown, Modifiers::NONE) {
            f = (f + 1).min(last);
        }
        if key(Key::ArrowUp, Modifiers::NONE) {
            f = f.saturating_sub(1);
        }
        if key(Key::PageDown, Modifiers::NONE) {
            f = (f + page).min(last);
        }
        if key(Key::PageUp, Modifiers::NONE) {
            f = f.saturating_sub(page);
        }
        if key(Key::Home, Modifiers::NONE) {
            f = 0;
        }
        if key(Key::End, Modifiers::NONE) {
            f = last;
        }
        let toggle_row = |app: &mut App, v: Vis| {
            if !editable {
                return;
            }
            match v {
                Vis::Link(i) if app.model.links[i].can_tick() => {
                    let on = app.model.links[i].tri() != Tri::All;
                    app.tick_link(i, on);
                }
                Vis::Kid(i, j) => {
                    let on = !app.model.links[i].kids_on.get(j).copied().unwrap_or(false);
                    app.model.links[i].set_kid(j, on);
                }
                Vis::Link(_) => {}
            }
        };
        let cur = vis[f];
        if key(Key::Space, Modifiers::NONE) {
            toggle_row(self, cur);
        }
        if key(Key::Enter, Modifiers::NONE) {
            match cur {
                Vis::Link(i) if self.model.links[i].is_folder() => {
                    let r = &mut self.model.links[i];
                    r.open = !r.open;
                }
                v => toggle_row(self, v),
            }
        }
        if key(Key::ArrowRight, Modifiers::NONE) {
            if let Vis::Link(i) = cur {
                if self.model.links[i].is_folder() {
                    self.model.links[i].open = true;
                }
            }
        }
        if key(Key::ArrowLeft, Modifiers::NONE) {
            match cur {
                Vis::Kid(i, _) => f = vis.iter().position(|v| *v == Vis::Link(i)).unwrap_or(f),
                Vis::Link(i) => self.model.links[i].open = false,
            }
        }
        if key(Key::A, Modifiers::COMMAND) && editable {
            self.tick_shown(true);
        }
        self.link_focus = f;
        if f == before {
            return None;
        }
        // Bring the focus row into view.
        let (top, bottom) = (f as f32 * LROW_H, (f + 1) as f32 * LROW_H);
        if top < offset {
            Some(top)
        } else if bottom > offset + view_h {
            Some(bottom - view_h)
        } else {
            None
        }
    }

    /// One list row: checkbox, two-line text, status and size.
    #[allow(clippy::too_many_arguments)]
    fn link_row(
        &self,
        ui: &mut Ui,
        p: &Pal,
        rect: Rect,
        index: usize,
        v: Vis,
        cols: (f32, f32, f32, f32),
        editable: bool,
        now: Instant,
        act: &mut dyn FnMut(RowAct),
    ) {
        let (c_cb, c_text, c_stat, c_size) = cols;
        let kid = matches!(v, Vis::Kid(..));
        let (link_i, kid_j) = match v {
            Vis::Link(i) => (i, None),
            Vis::Kid(i, j) => (i, Some(j)),
        };
        let r: &LinkRow = &self.model.links[link_i];
        let look = self.link_look(p, v, now);
        let lt = Layout::left_to_right(Align::Center);
        let at = |x: f32, wd: f32| Rect::from_min_size(pos2(rect.left() + x, rect.top()), vec2(wd, rect.height()));

        // The whole row: a click focuses the list, right-click offers `Copy link`, hover shows where it came from.
        let row_resp = ui.interact(rect, Id::unique(("link_row", index)), Sense::CLICK);
        if row_resp.clicked() || row_resp.secondary_clicked() {
            act(RowAct::Focus(index));
        }
        let url = r.link.url.clone();
        let row_resp = row_resp.on_hover_ui(|ui| {
            ui.set_max_width(520.0);
            ui.label(RichText::new(&url).monospace().size(T_MONO));
            for post in r.link.posts.iter().take(10) {
                let date = post.published.as_deref().and_then(|d| d.get(..10)).unwrap_or("undated");
                ui.label(dim(p, format!("{date} · {}", post.title)));
            }
            if r.link.posts.len() > 10 {
                ui.label(dim(p, format!("… and {} more posts", r.link.posts.len() - 10)));
            }
        });
        row_resp.context_menu(|ui| {
            if ui.button("Copy link").clicked() {
                ui.ctx().copy_text(url.clone());
                ui.close();
            }
        });

        // checkbox
        let (tri, can_tick) = match v {
            Vis::Link(_) => (r.tri(), r.can_tick()),
            Vis::Kid(_, j) => (if r.kids_on.get(j).copied().unwrap_or(false) { Tri::All } else { Tri::None }, true),
        };
        ui.scope_builder(UiBuilder::new().max_rect(at(if kid { 28.0 } else { 8.0 }, c_cb)).layout(lt), |ui| {
            let label = if kid { "File in folder" } else { "Link" };
            let resp = row_check(ui, tri, editable && can_tick, label);
            if resp.clicked() {
                act(RowAct::Toggle(v, tri != Tri::All));
                act(RowAct::Focus(index));
            }
            if !can_tick {
                let why = match &r.check {
                    Check::Dead(d) => r.ext_host().map(|h| format!("{}. Nothing to download.", d.text(h))),
                    _ => match &r.link.kind {
                        LinkKind::Unsupported { why } => {
                            Some(format!("{why}. The link is still included in Export list."))
                        }
                        _ => None,
                    },
                };
                if let Some(w) = why {
                    resp.on_hover_text(w);
                }
            }
        });

        // text column
        let indent = if kid { 24.0 } else { 0.0 };
        let (tx, tw) = (c_cb + 4.0 + indent, c_text - 4.0 - indent);
        let text_rect = at(tx, tw).shrink2(vec2(0.0, 5.0));
        let folder = !kid && r.is_folder();
        if folder {
            let hit = ui.interact(at(tx, tw), Id::unique(("link_open", index)), Sense::CLICK);
            if hit.clicked() {
                act(RowAct::Expand(link_i));
                act(RowAct::Focus(index));
            }
        }
        ui.scope_builder(UiBuilder::new().max_rect(text_rect).layout(Layout::top_down(Align::Min)), |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.spacing_mut().interact_size.y = 18.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if folder {
                    let (cr, _) = ui.allocate_exact_size(vec2(14.0, 18.0), Sense::hover());
                    chevron(ui, p.ink_dim, r.open, cr.center());
                }
                match v {
                    Vis::Kid(_, j) => {
                        let path = match r.resolved().map(|x| &x.kind) {
                            Some(ResolvedKind::Folder { children, .. }) => {
                                children.get(j).map(|c| c.path()).unwrap_or_default()
                            }
                            _ => String::new(),
                        };
                        path_label(ui, p, &path, tw - 6.0);
                    }
                    Vis::Link(_) => {
                        let (name, state) = link_title(r);
                        let mut t = RichText::new(name);
                        match state {
                            Title::Real => {}
                            Title::Hint => t = t.color(p.ink_dim),
                            Title::Path => t = t.monospace().size(T_MONO).color(p.ink_dim),
                        }
                        if !r.link.downloadable() {
                            t = t.color(p.ink_dim);
                        }
                        ui.add(egui::Label::new(t).truncate());
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if folder {
                    ui.add_space(18.0);
                }
                match v {
                    Vis::Link(_) => {
                        ui.label(RichText::new(r.link.host.name()).size(T_SMALL).color(p.ink));
                        ui.label(dim(p, "·"));
                        let src = r.link.source();
                        let date = src.published.as_deref().and_then(|d| d.get(..10)).unwrap_or("undated");
                        ui.add(egui::Label::new(dim(p, format!("{date} · {}", src.title))).truncate());
                        if r.link.posts.len() > 1 {
                            ui.label(dim(p, format!("+{} more", r.link.posts.len() - 1)));
                        }
                    }
                    Vis::Kid(_, j) => {
                        let word = self.kid_type_word(r, j);
                        ui.label(dim(p, word));
                    }
                }
            });
        });

        // status column
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(at(c_cb + c_text, c_stat).shrink2(vec2(2.0, 5.0)))
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.spacing_mut().interact_size.y = 18.0;
                let resp = ui
                    .horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        paint_icon(ui, p, look.ic, look.color);
                        let mut t = RichText::new(&look.word).color(look.color);
                        if look.bar.is_some() {
                            t = t.monospace().size(T_MONO);
                        }
                        ui.label(t);
                    })
                    .response;
                if let Some((f, c)) = look.bar {
                    ui.scope(|ui| {
                        ui.spacing_mut().interact_size.y = 6.0;
                        ui.add(
                            egui::ProgressBar::new(f)
                                .desired_width(c_stat - 24.0)
                                .fill(c)
                                .corner_radius(CornerRadius::same(3)),
                        );
                    });
                } else if let Some(l2) = &look.line2 {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if look.info {
                            ui.label(RichText::new("ⓘ").size(T_SMALL).color(p.accent));
                        } else {
                            ui.add_space(22.0);
                        }
                        ui.add(egui::Label::new(dim(p, l2)).truncate());
                    });
                }
                if let Some(tip) = &look.tooltip {
                    resp.on_hover_text(tip);
                }
            },
        );

        // size column
        let size = self.link_size_text(r, kid_j);
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(at(c_cb + c_text + c_stat, c_size).shrink2(vec2(4.0, 0.0)))
                .layout(Layout::right_to_left(Align::Center)),
            |ui| {
                let dim_size = size.1 || !r.link.downloadable();
                ui.label(num(size.0).color(if dim_size { p.ink_dim } else { p.ink }));
            },
        );
    }

    fn kid_type_word(&self, r: &LinkRow, j: usize) -> &'static str {
        match r.resolved().map(|x| &x.kind) {
            Some(ResolvedKind::Folder { children, .. }) => {
                children.get(j).map(|c| links_state::type_label(&c.name, c.mime.as_deref())).unwrap_or("Other")
            }
            _ => "Other",
        }
    }

    /// `(text, dim)`: the size cell. Dim while some size is unknown.
    fn link_size_text(&self, r: &LinkRow, kid: Option<usize>) -> (String, bool) {
        if !r.can_tick() {
            return ("—".into(), true);
        }
        match (kid, r.resolved()) {
            (Some(j), Some(res)) => match &res.kind {
                ResolvedKind::Folder { children, .. } => match children.get(j).and_then(|c| c.size) {
                    Some(s) => (human_size(s), false),
                    None => ("—".into(), true),
                },
                ResolvedKind::File => ("—".into(), true),
            },
            (None, Some(res)) => {
                let (bytes, unknown) = r.bytes_all();
                match &res.kind {
                    ResolvedKind::File => res.size.map_or(("—".into(), true), |s| (human_size(s), false)),
                    ResolvedKind::Folder { .. } if bytes == 0 && unknown => ("—".into(), true),
                    ResolvedKind::Folder { .. } => (size_label(bytes, unknown), unknown),
                }
            }
            _ => ("—".into(), true),
        }
    }

    /// Glyph, word and second line of a row, from its check state or (once a run exists) its jobs.
    fn link_look(&self, p: &Pal, v: Vis, now: Instant) -> Look {
        let m = &self.model;
        let (i, kid) = match v {
            Vis::Link(i) => (i, None),
            Vis::Kid(i, j) => (i, Some(j)),
        };
        let r = &m.links[i];
        let host = r.ext_host();
        let jobs: Vec<&Row> = match (kid, m.rows.is_empty()) {
            (_, true) => Vec::new(),
            (Some(j), false) => m.link_job_rows(i, Some(Some(j))),
            (None, false) if r.is_folder() => m.link_job_rows(i, None),
            (None, false) => m.link_job_rows(i, Some(None)),
        };
        if jobs.is_empty() {
            return self.idle_look(p, r, kid);
        }
        // ---- a run exists and this row has jobs in it
        let partial = r.resolved().is_some_and(|x| x.partial());
        let n = jobs.len();
        let (mut done, mut failed, mut running, mut skipped) = (0, 0, 0, 0);
        let (mut bytes, mut total, mut total_known) = (0u64, 0u64, true);
        let mut retry: Option<u64> = None;
        for j in &jobs {
            bytes += j.done;
            match j.total {
                Some(t) => total += t,
                None => total_known = false,
            }
            match &j.state {
                RowState::Done => done += 1,
                RowState::Skipped => {
                    done += 1;
                    skipped += 1;
                }
                RowState::Failed { .. } => failed += 1,
                RowState::Active => running += 1,
                RowState::Retrying { until, .. } => {
                    running += 1;
                    retry = Some(until.saturating_duration_since(now).as_secs());
                }
                RowState::Queued | RowState::Stopped => {}
            }
        }
        let cooling = host.and_then(|h| m.host_cooldown_secs(h, now));
        if done + failed == n {
            if failed > 0 {
                let line2 = if n == 1 {
                    jobs.iter()
                        .find_map(|j| match &j.state {
                            RowState::Failed { error, .. } => Some(explain_failure(error)),
                            _ => None,
                        })
                        .unwrap_or_default()
                } else {
                    format!("{done} saved · {failed} failed")
                };
                return Look { info: partial && n > 1, ..Look::simple(Ic::Cross, p.bad, "Failed", Some(line2)) };
            }
            let line2 = if n > 1 {
                if partial {
                    format!("Saved {n} · maybe more")
                } else {
                    format!("Saved · {n} files")
                }
            } else if skipped == 1 {
                "Already saved".to_string()
            } else if jobs[0].renamed {
                format!("Saved as {}", jobs[0].name.rsplit('/').next().unwrap_or(&jobs[0].name))
            } else {
                format!("Saved · {}", self.done_type_word(r, kid, jobs[0]))
            };
            return Look { info: partial && n > 1, ..Look::simple(Ic::Check, p.ok, "Done", Some(line2)) };
        }
        if let (Some(secs), true) = (cooling, running + done + failed < n || running == 0) {
            let host = host.map(|h| h.name()).unwrap_or("Host");
            let _ = secs;
            return Look::simple(Ic::Dots, p.warn, "Waiting", Some(format!("{host} cool-down")));
        }
        if m.phase == Phase::Paused && running > 0 {
            return Look::simple(Ic::Pause, p.warn, "Paused", None);
        }
        if let Some(s) = retry {
            return Look::simple(Ic::Dots, p.warn, "Waiting", Some(format!("Retrying in {s} s")));
        }
        if running > 0 || done > 0 {
            let f = if total_known && total > 0 { bytes as f32 / total as f32 } else { done as f32 / n as f32 };
            let f = f.clamp(0.0, 1.0);
            let tooltip = (partial && n > 1).then(|| INCOMPLETE_TIP.replace("{n}", &n.to_string()));
            return Look {
                bar: Some((f, p.accent)),
                tooltip,
                ..Look::simple(Ic::Down, p.accent, format!("{:.0}%", f * 100.0), None)
            };
        }
        let line2 = if n > 1 { format!("{n} files") } else { self.done_type_word(r, kid, jobs[0]).to_string() };
        Look::simple(if m.phase == Phase::Cancelled { Ic::Pause } else { Ic::Dot }, p.ink_dim, "Queued", Some(line2))
    }

    /// Type word for a saved file: the real name and Content-Type the server sent, never the link text.
    fn done_type_word(&self, r: &LinkRow, kid: Option<usize>, job: &Row) -> &'static str {
        if let (Some(j), Some(res)) = (kid, r.resolved()) {
            // A Drive child: the listing's name and MIME decide (Drive says octet-stream for everything).
            return self.kid_type_word_of(res, j);
        }
        let name =
            job.real_name.clone().or_else(|| r.resolved().map(|x| x.name.clone())).unwrap_or_else(|| job.name.clone());
        links_state::type_label(
            &name,
            job.content_type.as_deref().or(r.resolved().and_then(|x| x.content_type.as_deref())),
        )
    }

    fn kid_type_word_of(&self, res: &pawchive_core::external::Resolved, j: usize) -> &'static str {
        match &res.kind {
            ResolvedKind::Folder { children, .. } => {
                children.get(j).map(|c| links_state::type_label(&c.name, c.mime.as_deref())).unwrap_or("Other")
            }
            ResolvedKind::File => "Other",
        }
    }

    /// The look before any download: from what checking found out.
    fn idle_look(&self, p: &Pal, r: &LinkRow, kid: Option<usize>) -> Look {
        if let Some(j) = kid {
            let word = r.resolved().map_or("Other", |res| self.kid_type_word_of(res, j));
            return Look::simple(Ic::Dot, p.ink_dim, "Ready", Some(word.to_string()));
        }
        if let LinkKind::Unsupported { why } = &r.link.kind {
            return Look::simple(Ic::NoEntry, p.ink_dim, "Not downloadable", Some(why.clone()));
        }
        match &r.check {
            Check::Dead(d) => {
                let text = r.ext_host().map(|h| d.text(h)).unwrap_or_default();
                Look::simple(Ic::Cross, p.bad, "Dead link", Some(text))
            }
            Check::Checking => {
                let folder = matches!(r.link.kind, LinkKind::DriveFolder { .. });
                Look::simple(Ic::Spin, p.accent, "Checking…", folder.then(|| "Listing folder".to_string()))
            }
            Check::Resolved(res) => match &res.kind {
                ResolvedKind::File => Look::simple(
                    Ic::Dot,
                    p.ink_dim,
                    "Ready",
                    Some(links_state::type_label(&res.name, res.content_type.as_deref()).to_string()),
                ),
                ResolvedKind::Folder { children, partial } => {
                    let n = children.len();
                    if *partial {
                        Look {
                            info: true,
                            tooltip: Some(INCOMPLETE_TIP.replace("{n}", &n.to_string())),
                            ..Look::simple(Ic::Dot, p.ink_dim, "Ready", Some(format!("At least {n} files")))
                        }
                    } else {
                        Look::simple(
                            Ic::Dot,
                            p.ink_dim,
                            "Ready",
                            Some(if n == 1 { "1 file".into() } else { format!("{n} files") }),
                        )
                    }
                }
            },
            Check::Unavailable(msg) => {
                Look::simple(Ic::Dot, p.ink_dim, "Not checked", Some(unavailable_text(msg).to_string()))
            }
            Check::NotChecked => Look::simple(Ic::Dot, p.ink_dim, "Not checked", None),
        }
    }
}

enum RowAct {
    Toggle(Vis, bool),
    Expand(usize),
    Focus(usize),
}

#[derive(Clone, Copy)]
enum Title {
    Real,
    Hint,
    Path,
}

/// Line 1 of a link: the real name, else the name read from the URL, else the shortened URL path.
fn link_title(r: &LinkRow) -> (String, Title) {
    if let Some(res) = r.resolved() {
        return (res.name.clone(), Title::Real);
    }
    if let Some(h) = &r.link.name_hint {
        return (h.clone(), Title::Hint);
    }
    let path = r
        .link
        .url
        .split_once("://")
        .map_or(r.link.url.as_str(), |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    let path = path.split(['?', '#']).next().unwrap_or("");
    (format!("/{path}"), Title::Path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pawchive_core::links::{Host, Link, PostRef};

    fn row(url: &str, hint: Option<&str>) -> LinkRow {
        LinkRow::new(Link {
            id: 0,
            url: url.into(),
            host: Host::Mega,
            kind: LinkKind::Unsupported { why: "x".into() },
            name_hint: hint.map(str::to_string),
            posts: vec![PostRef { id: "1".into(), title: "t".into(), published: None }],
        })
    }

    #[test]
    fn line_one_falls_back_from_name_to_hint_to_shortened_path() {
        let (t, s) = link_title(&row("https://mega.nz/folder/Kp3a9QxZ#key", None));
        assert_eq!((t.as_str(), matches!(s, Title::Path)), ("/folder/Kp3a9QxZ", true));
        let (t, s) = link_title(&row("https://x.test/a/b.zip", Some("b.zip")));
        assert_eq!((t.as_str(), matches!(s, Title::Hint)), ("b.zip", true));
    }

    #[test]
    fn unavailable_checks_say_why_when_they_can() {
        assert_eq!(
            unavailable_text("gave up after 5 tries: request failed: operation timed out"),
            "Couldn't check (timed out)"
        );
        assert_eq!(unavailable_text("Google Drive answered HTTP 500"), "Couldn't check");
    }
}
