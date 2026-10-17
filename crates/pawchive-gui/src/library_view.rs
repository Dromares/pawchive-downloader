//! The saved-profile screens of `ui-spec-library`: the list, the status cards of an update, the Posts
//! tab with its labels, and the small widgets they need. Drawing only: clicks come back as [`Action`]s.

use std::time::Instant;

use eframe::egui::{
    self, pos2, vec2, Align, Color32, CornerRadius, Frame, Layout, Margin, Rect, Response, RichText, ScrollArea, Sense,
    Shape, Stroke, StrokeKind, Ui, UiBuilder,
};
use pawchive_core::library::PostLabel;
use pawchive_core::session::{Loaded, UpdateKind, UpdateStep};

use crate::app::{Action, App};
use crate::format::{comma, files as files_word};
use crate::library_state::{self as ls, PostFilter};
use crate::state::{Lib, Notice, Phase};
use crate::theme::*;
use crate::view::{card_head, size_label, Lead};
use crate::widgets::{bar, card, dim, indeterminate, num, Cols};

const LIST_ROW_H: f32 = 56.0;
const POST_ROW_H: f32 = 28.0;
const PILL_H: f32 = 20.0;
const WIDE: f32 = 640.0;

/// Seconds-offset of the user's zone, for the pure time functions.
fn off(t: i64) -> i64 {
    ls::utc_offset(t)
}

// ---- small widgets ---------------------------------------------------------------------------------

/// An outline pill: the word carries the meaning, the colour is redundant (WCAG 1.4.1).
pub(crate) fn pill(ui: &mut Ui, text: &str, color: Color32, tip: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), egui::FontId::proportional(T_SMALL), color);
    let size = vec2(galley.size().x + 14.0, PILL_H);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, format!("Label: {text}")));
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(10),
        Stroke::new(1.0, color.gamma_multiply(0.55)),
        StrokeKind::Inside,
    );
    ui.painter().galley(pos2(rect.left() + 7.0, rect.center().y - galley.size().y / 2.0), galley, color);
    resp.on_hover_text(tip)
}

/// `‹ Saved profiles`: accent text with a drawn chevron (U+2039 is not in every bundled font).
pub(crate) fn back_link(ui: &mut Ui, p: &Pal, enabled: bool, reason: &str) -> Response {
    let color = if enabled { p.accent } else { p.ink_dim };
    let galley =
        ui.painter().layout_no_wrap("Saved profiles".to_string(), egui::FontId::proportional(T_SMALL + 0.5), color);
    let size = vec2(galley.size().x + 16.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, "Saved profiles"));
    let mid = rect.left() + 5.0;
    let c = rect.center().y;
    ui.painter().add(Shape::line(
        vec![pos2(mid + 3.0, c - 4.5), pos2(mid - 1.0, c), pos2(mid + 3.0, c + 4.5)],
        Stroke::new(1.6, color),
    ));
    ui.painter().galley(pos2(rect.left() + 14.0, c - galley.size().y / 2.0), galley, color);
    if enabled && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().hline(rect.left() + 14.0..=rect.right() - 2.0, c + 8.0, Stroke::new(1.0, color));
    }
    if enabled {
        resp.on_hover_text("Back to your saved profiles. Nothing is lost.")
    } else {
        resp.on_hover_text(reason)
    }
}

fn plural_posts(n: usize) -> String {
    if n == 1 {
        "1 post".to_string()
    } else {
        format!("{} posts", comma(n))
    }
}

/// `2026-10-17T13:02:44` -> `2026-10-17 13:02`.
pub(crate) fn site_time(s: &str) -> String {
    s.get(..16).map_or_else(|| s.to_string(), |t| t.replace('T', " "))
}

// ---- the saved list --------------------------------------------------------------------------------

impl App {
    pub(crate) fn saved_list(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        let profiles = self.model.profiles().to_vec();
        let now = ls::unix_now();
        let w = ui.available_width();
        let narrow = w < WIDE;
        ui.add_space(S2);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Saved profiles").size(T_CARD).strong());
            ui.label(num(profiles.len().to_string()).color(p.ink_dim));
        });
        ui.add(
            egui::Label::new(dim(
                p,
                "Open a profile to update it or download what's new. No need to paste the link again.",
            ))
            .wrap(),
        );
        ui.add_space(S2);

        // columns: Profile flex | Posts 84 | Last scanned 132 (+16 gap) | actions 168
        let widths_for = |row_w: f32| -> Vec<f32> {
            if narrow {
                vec![(row_w - 168.0).max(100.0), 0.0, 0.0, 168.0]
            } else {
                vec![(row_w - 84.0 - 148.0 - 168.0).max(120.0), 84.0, 148.0, 168.0]
            }
        };
        let head = Cols::new(ui, 28.0, widths_for(w));
        ui.painter().rect_filled(head.rect(), CornerRadius { nw: RADIUS, ne: RADIUS, sw: 0, se: 0 }, p.sunken);
        head.put(ui, 0, false, |ui| {
            ui.label(dim(p, "Profile"));
        });
        if !narrow {
            head.put(ui, 1, true, |ui| {
                ui.label(dim(p, "Posts"));
            });
            head.put(ui, 2, false, |ui| {
                ui.add_space(16.0);
                ui.label(dim(p, "Last scanned"));
            });
        }
        let avail = ui.available_height();
        let mut esc = false;
        Frame::new()
            .stroke(Stroke::new(1.0, p.line))
            .corner_radius(CornerRadius { nw: 0, ne: 0, sw: RADIUS, se: RADIUS })
            .show(ui, |ui| {
                ui.set_width(w - 2.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                ScrollArea::vertical().auto_shrink([false, true]).max_height((avail - 90.0).max(120.0)).show(
                    ui,
                    |ui| {
                        for (i, rec) in profiles.iter().enumerate() {
                            let name = rec
                                .name
                                .clone()
                                .unwrap_or_else(|| format!("{} {}", rec.creator.service.as_str(), rec.creator.id));
                            let when = rec
                                .last_scanned_at
                                .map(|t| ls::last_scanned(t, now, off))
                                .unwrap_or_else(|| "—".into());
                            if self.remove_confirm.as_ref() == Some(&rec.creator) {
                                esc |= self.confirm_row(ui, p, w, narrow, &name, rec.gone, acts);
                                continue;
                            }
                            let (rect, _) =
                                ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::hover());
                            if i % 2 == 1 {
                                ui.painter().rect_filled(rect, 0.0, p.surface);
                            }
                            let cols = Cols::at(rect, widths_for(rect.width()));
                            cols.put(ui, 0, false, |ui| {
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.add_space(9.0);
                                    ui.add(egui::Label::new(RichText::new(&name).size(T_BODY).strong()).truncate());
                                    let sub = if narrow {
                                        format!("{} · {} · {when}", rec.creator.service.as_str(), rec.creator.id)
                                    } else {
                                        format!("{} · {}", rec.creator.service.as_str(), rec.creator.id)
                                    };
                                    ui.add(egui::Label::new(dim(p, sub)).truncate());
                                });
                            });
                            if !narrow {
                                cols.put(ui, 1, true, |ui| {
                                    ui.label(num(comma(rec.posts)));
                                });
                                cols.put(ui, 2, false, |ui| {
                                    ui.add_space(16.0);
                                    let r = ui.label(RichText::new(&when).size(T_BODY));
                                    if let Some(t) = rec.last_scanned_at {
                                        let mut tip = format!("Last scanned {}", ls::full_time(t, off));
                                        if let Some(full) =
                                            rec.last_full_read_at.filter(|f| rec.last_scanned_at != Some(*f))
                                        {
                                            tip.push_str(&format!(
                                                "\nWhole post list last read {}",
                                                ls::full_time(full, off)
                                            ));
                                        }
                                        r.on_hover_text(tip);
                                    }
                                });
                            }
                            cols.put(ui, 3, false, |ui| {
                                // Exact spacing: any slack here would widen the row, and the scroll area the next one.
                                ui.spacing_mut().item_spacing.x = 0.0;
                                ui.add_space(168.0 - 8.0 - 72.0 - 8.0 - 76.0);
                                if ui
                                    .add(egui::Button::new(RichText::new("Open").strong()).min_size(vec2(72.0, 28.0)))
                                    .on_hover_text("Open from the saved copy. No request is made.")
                                    .clicked()
                                {
                                    acts.push(Action::OpenSaved(i));
                                }
                                ui.add_space(8.0);
                                if ui
                                    .add(egui::Button::new("Remove").min_size(vec2(76.0, 28.0)))
                                    .on_hover_text("Remove from the list. Downloaded files are kept.")
                                    .clicked()
                                {
                                    acts.push(Action::AskRemove(i));
                                }
                            });
                        }
                    },
                );
            });
        ui.add_space(S2);
        match &self.model.removed_note {
            Some(name) => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("✓").color(p.ok));
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("Removed “{name}”. Downloaded files were kept.")).size(T_SMALL),
                        )
                        .wrap(),
                    );
                });
            }
            None => {
                ui.add(
                    egui::Label::new(dim(
                        p,
                        "Removing a profile from this list never deletes the files you downloaded. To add another profile, paste its link on the left.",
                    ))
                    .wrap(),
                );
            }
        }
        if esc || (self.remove_confirm.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape))) {
            acts.push(Action::KeepProfile);
        }
    }

    /// The row that asks before a profile is removed. Returns true if Esc was pressed.
    #[allow(clippy::too_many_arguments)]
    fn confirm_row(
        &mut self,
        ui: &mut Ui,
        p: &Pal,
        w: f32,
        narrow: bool,
        name: &str,
        gone: usize,
        acts: &mut Vec<Action>,
    ) -> bool {
        let h = (if gone > 0 { 132.0 } else { 110.0 }) + if narrow { 18.0 } else { 0.0 };
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
        ui.painter().rect_filled(rect, 0.0, p.sunken);
        ui.painter().rect_filled(Rect::from_min_size(rect.left_top(), vec2(4.0, rect.height())), 0.0, p.line_strong);
        let inner = Rect::from_min_max(rect.left_top() + vec2(16.0, 8.0), rect.right_bottom() - vec2(12.0, 6.0));
        let mut keep_id = None;
        // `new_child`, not `scope_builder`: a scope would move the parent's cursor back up to the
        // end of what it drew, and the next row would overlap this one.
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
        {
            let ui = &mut child;
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.add(egui::Label::new(RichText::new(format!("Remove “{name}” from your list?")).strong()).truncate());
            ui.add(
                egui::Label::new(dim(
                    p,
                    "Your downloaded files stay in your folder. The saved post list and download history are deleted: adding it again starts as a first scan, and downloading again saves new copies beside the old files (name (2)).",
                ))
                .wrap(),
            );
            if gone > 0 {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("⚠").color(p.warn));
                    ui.label(
                        RichText::new(format!(
                            "{gone} {} marked Gone from site will be forgotten.",
                            if gone == 1 { "post" } else { "posts" }
                        ))
                        .size(T_SMALL),
                    );
                });
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = S2;
                let keep = ui.add(egui::Button::new(RichText::new("Keep").strong()).min_size(vec2(84.0, 28.0)));
                keep_id = Some(keep.id);
                if keep.clicked() {
                    acts.push(Action::KeepProfile);
                }
                if ui.add(egui::Button::new("Remove").min_size(vec2(84.0, 28.0))).clicked() {
                    acts.push(Action::ConfirmRemove);
                }
            });
        }
        // Keep is the safe default: it gets the focus when the row appears.
        if let Some(id) = keep_id {
            if !self.confirm_focused {
                self.confirm_focused = true;
                ui.memory_mut(|m| m.request_focus(id));
            }
        }
        let _ = w;
        false
    }

    /// First run (or an emptied library): the guide's card.
    pub(crate) fn first_run_card(&self, ui: &mut Ui, p: &Pal) {
        ui.add_space(S3);
        Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.line))
            .corner_radius(CornerRadius::same(RADIUS + 2))
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(440.0);
                ui.label(RichText::new("No saved profiles yet").strong());
                ui.add(
                    egui::Label::new(dim(
                        p,
                        "A profile is saved the first time you scan it. After that, open it from here and press Update to see what changed, without pasting the link again.",
                    ))
                    .wrap(),
                );
            });
    }

    /// The library file could not be opened: say so above the guide.
    pub(crate) fn library_unavailable_card(&self, ui: &mut Ui, p: &Pal) {
        let Lib::Unavailable(why) = &self.model.lib else { return };
        Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.warn))
            .corner_radius(CornerRadius::same(RADIUS + 2))
            .inner_margin(Margin::symmetric(18, 14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("⚠").color(p.warn).size(T_CARD));
                    ui.label(RichText::new("Your library couldn't be opened").size(T_CARD).strong());
                });
                ui.add(egui::Label::new(why.as_str()).wrap());
                ui.add(
                    egui::Label::new(dim(
                        p,
                        "You can still scan and download, but profiles won't be saved or listed until this is fixed. The library file hasn't been changed or deleted.",
                    ))
                    .wrap(),
                );
            });
    }

    // ---- step 1 -----------------------------------------------------------------------------------

    /// The line under the link: what is open (with Full rescan at its right end), or that the pasted
    /// link is already saved. `None` when the plain v0.2.0 hint applies.
    pub(crate) fn saved_hint(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) -> bool {
        let busy_update = self.model.update.is_some();
        if let (Some(saved), true) = (&self.model.saved, self.model.phase != Phase::Empty) {
            let rec = &saved.record;
            let name =
                rec.name.clone().unwrap_or_else(|| format!("{} {}", rec.creator.service.as_str(), rec.creator.id));
            let mut line =
                format!("{name} · {} {} · {}", rec.creator.service.as_str(), rec.creator.id, plural_posts(rec.posts));
            if let Some(u) = &self.model.update {
                line.push_str(match u.step {
                    UpdateStep::Checking => " · checking…",
                    _ => " · updating…",
                });
            }
            let phase = self.model.phase;
            let can = matches!(phase, Phase::Ready | Phase::Cancelled | Phase::Finished);
            let reason = if busy_update || phase == Phase::Scanning {
                "A scan is already running."
            } else {
                "Wait for the download to finish, or cancel it."
            };
            let secs = ls::read_seconds(rec.posts, self.settings.delay_ms);
            // The row must be 24 pt: `interact_size` is set on the parent, because inside the horizontal
            // closure it is too late and the row stays 30 pt (ui-spec-library §1.4).
            ui.scope(|ui| {
                ui.spacing_mut().interact_size.y = 24.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = S2;
                    let right = 92.0 + S2;
                    // The text gets what the button leaves; a truncating label would take it all.
                let text_w = (ui.available_width() - right).max(40.0);
                ui.allocate_ui_with_layout(vec2(text_w, 24.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(dim(p, &line)).truncate()).on_hover_text(&line);
                });
                let b = ui.add_enabled(can, egui::Button::new(RichText::new("Full rescan").size(T_SMALL)).min_size(vec2(92.0, 24.0)));
                    let b = if can {
                        b.on_hover_text(format!(
                            "Reads every post again, even if Pawchive says nothing changed. About {secs} s for this profile."
                        ))
                    } else {
                        b.on_disabled_hover_text(reason)
                    };
                    if b.clicked() {
                        acts.push(Action::FullRescan);
                    }
                });
            });
            return true;
        }
        if let (Some(rec), Phase::Empty) = (self.typed_saved(), self.model.phase) {
            let when =
                rec.last_scanned_at.map(|t| ls::last_scanned(t, ls::unix_now(), off)).unwrap_or_else(|| "never".into());
            ui.add(
                egui::Label::new(dim(
                    p,
                    format!("Already in your library · last scanned {when}. Update checks what changed."),
                ))
                .wrap(),
            );
            return true;
        }
        false
    }

    // ---- status cards -----------------------------------------------------------------------------

    /// Cards of the saved-profile flow. `false` when the plain card applies.
    pub(crate) fn library_card(&mut self, ui: &mut Ui, p: &Pal, now: Instant) -> bool {
        if let (Phase::Scanning, Some(u)) = (self.model.phase, self.model.update) {
            self.update_card(ui, p, now, u);
            return true;
        }
        if self.model.phase != Phase::Ready {
            return false;
        }
        if let Some(n) = self.model.notice.clone() {
            self.notice_card(ui, p, &n);
            return true;
        }
        let Some(how) = self.model.saved.as_ref().map(|s| s.how.clone()) else { return false };
        match how {
            Loaded::First => false,
            Loaded::Opened | Loaded::Updated(_) => {
                self.saved_card(ui, p, &how);
                true
            }
        }
    }

    fn update_card(&mut self, ui: &mut Ui, p: &Pal, now: Instant, u: crate::state::UpdateRun) {
        let elapsed = crate::format::clock(self.model.scan_elapsed(now));
        let sp = self.model.scan_progress;
        let est = pawchive_core::session::est_pages(u.saved_posts);
        let full = u.kind == UpdateKind::Full;
        let pause = self.settings.delay_ms;
        let saving_long = u.saving_since.is_some_and(|t| now.saturating_duration_since(t).as_millis() >= 400);
        let when =
            u.last_scanned.map(|t| ls::last_scanned(t, ls::unix_now(), off)).unwrap_or_else(|| "the last scan".into());
        let t = ui.input(|i| i.time) as f32 * 0.6 % 2.0;
        let sweep = if t > 1.0 { 2.0 - t } else { t };
        card(ui, p, p.accent, |ui| {
            if saving_long {
                card_head(ui, p, Lead::Spinner, "Saving to your library…", Some(elapsed));
                bar(ui, 1.0, p.accent);
                ui.label("Everything is read. Writing it to your library in one step.");
                ui.label(dim(p, "Your saved copy is replaced only when this finishes."));
                return;
            }
            match u.step {
                UpdateStep::Checking => {
                    card_head(ui, p, Lead::Spinner, "Checking the profile…", Some(elapsed));
                    indeterminate(ui, p, sweep);
                    ui.add(
                        egui::Label::new(format!("One request to Pawchive: has anything changed since {when}?"))
                            .truncate(),
                    );
                    ui.add(
                        egui::Label::new(dim(
                            p,
                            "If nothing changed, that is the whole update. Your saved copy isn't touched.",
                        ))
                        .truncate(),
                    );
                }
                UpdateStep::Reading | UpdateStep::Page(_) | UpdateStep::Saving => {
                    card_head(ui, p, Lead::Spinner, "Reading the post list…", Some(elapsed));
                    bar(ui, ls::reading_fraction(sp.pages, est), p.accent);
                    ui.label(num(ls::reading_counter(sp.pages.max(1), est, sp.posts, full)));
                    if est > 0 {
                        let secs = est as u64 * pause / 1000;
                        ui.add(
                            egui::Label::new(dim(
                                p,
                                format!("About {secs} s at one request per second. Your saved copy isn't changed until this finishes."),
                            ))
                            .truncate(),
                        );
                    } else {
                        ui.add(
                            egui::Label::new(dim(p, "Your saved copy isn't changed until this finishes.")).truncate(),
                        );
                    }
                }
                UpdateStep::CheckingMissing { done, total } => {
                    card_head(ui, p, Lead::Spinner, "Checking posts missing from the list…", Some(elapsed));
                    bar(ui, if total == 0 { 0.0 } else { done as f32 / total as f32 }, p.accent);
                    ui.label(num(ls::missing_counter(done, total, pause)));
                    ui.add(
                        egui::Label::new(dim(
                            p,
                            "Each is checked on its own before it's marked Gone from site. Nothing is changed yet.",
                        ))
                        .truncate(),
                    );
                }
            }
        });
    }

    fn notice_card(&mut self, ui: &mut Ui, p: &Pal, n: &Notice) {
        let (posts, files) = self.model.saved.as_ref().map_or((0, 0), |s| (s.record.posts, self.model.files.len()));
        let scanned = self.model.saved.as_ref().and_then(|s| s.record.last_scanned_at);
        match n {
            Notice::Failed { page, cause, saving } => {
                card(ui, p, p.warn, |ui| {
                    let head = if *saving {
                        "Couldn't save the update".to_string()
                    } else {
                        format!("Update stopped at page {page}")
                    };
                    card_head(ui, p, Lead::Glyph("⚠", p.warn), &head, None);
                    let line = if *saving { cause.clone() } else { crate::state::explain_update_cause(cause) };
                    ui.add(egui::Label::new(line).truncate());
                    ui.add(
                        egui::Label::new(dim(
                            p,
                            "Nothing was changed. Your saved copy is intact. Update again starts over.",
                        ))
                        .truncate(),
                    );
                });
            }
            Notice::Cancelled => {
                card(ui, p, p.line_strong, |ui| {
                    card_head(ui, p, Lead::Glyph("■", p.ink_dim), "Update cancelled", None);
                    ui.label("Nothing was changed.");
                    let when =
                        scanned.map(|t| ls::last_scanned(t, ls::unix_now(), off)).unwrap_or_else(|| "earlier".into());
                    ui.add(
                        egui::Label::new(dim(
                            p,
                            format!(
                                "Your saved copy from {when} is intact: {} posts, {}.",
                                comma(posts),
                                files_word(files)
                            ),
                        ))
                        .truncate(),
                    );
                });
            }
        }
    }

    /// `n new files selected · size` (profile with history) or today's `N files selected · size`.
    pub(crate) fn selection_line(&mut self, reason_for: Option<usize>) -> String {
        let history = self.model.saved.as_ref().is_some_and(|s| s.has_history());
        if history {
            let n = self.new_only();
            let size = self.size_text(n.bytes, n.unsized_files, n.partial);
            let mut line = format!(
                "{} selected · {size}",
                if n.files == 1 { "1 new file".to_string() } else { format!("{} new files", comma(n.files)) }
            );
            if let Some(found) = reason_for {
                if n.files > found {
                    line.push_str(&format!(" · {} from earlier updates", comma(n.files - found)));
                } else if n.files < found {
                    line.push_str(&format!(
                        " · {} of the {} new files aren't selected",
                        comma(found - n.files),
                        comma(found)
                    ));
                }
            }
            line
        } else {
            let sel = self.selection();
            let mut line = format!(
                "{} selected · {}",
                files_word(sel.files),
                self.size_text(sel.bytes, sel.unsized_files, sel.partial)
            );
            if sel.thumbnails {
                line.push_str(" + thumbnails (size not measured)");
            }
            line
        }
    }

    /// `12.3 MB`, `≥ 12.3 MB` while sizes arrive, `size not measured` for a reopened profile.
    pub(crate) fn size_text(&self, bytes: u64, unsized_files: usize, partial: bool) -> String {
        if bytes == 0 && unsized_files > 0 && !self.sizing_running() {
            "size not measured".to_string()
        } else {
            size_label(bytes, partial)
        }
    }

    fn saved_card(&mut self, ui: &mut Ui, p: &Pal, how: &Loaded) {
        let Some(saved) = self.model.saved.clone() else { return };
        let rec = &saved.record;
        let files = self.model.files.len();
        let scanned =
            rec.last_scanned_at.map(|t| ls::last_scanned(t, ls::unix_now(), off)).unwrap_or_else(|| "never".into());
        match how {
            Loaded::Opened => {
                let line1 = self.selection_line(None);
                let line2 = match rec.last_change {
                    Some(c) => format!("Last scanned {scanned}. Last update found {}.", ls::change_line(&c)),
                    None => format!("Last scanned {scanned}. Press Update to check for changes."),
                };
                let head = format!("Saved profile — {}, {}", plural_posts(rec.posts), files_word(files));
                card(ui, p, p.ok, |ui| {
                    card_head(ui, p, Lead::Glyph("✓", p.ok), &head, None);
                    ui.add(egui::Label::new(line1).truncate());
                    ui.add(egui::Label::new(dim(p, line2)).truncate());
                });
            }
            Loaded::Updated(r) => {
                let zero = r.summary == Default::default();
                let head = ls::update_headline(&r.summary);
                let line1 = if zero && saved.has_history() {
                    let downloaded = saved.marks.iter().filter(|m| m.downloaded).count();
                    ls::zero_update_files_line(downloaded, self.new_only().files)
                } else {
                    self.selection_line(Some(r.summary.new_files))
                };
                let line2 = if zero && r.unchanged {
                    match &rec.site_updated {
                        Some(t) => format!(
                            "Pawchive reports no changes since {}. Full rescan checks every post anyway.",
                            site_time(t)
                        ),
                        None => "Pawchive reports no changes. Full rescan checks every post anyway.".to_string(),
                    }
                } else if zero {
                    format!("All {} posts were read and none changed.", comma(r.posts_seen))
                } else {
                    format!("Updated {scanned}. Open the Posts tab to see which posts changed.")
                };
                card(ui, p, p.ok, |ui| {
                    card_head(ui, p, Lead::Glyph("✓", p.ok), &head, None);
                    ui.add(egui::Label::new(line1).truncate());
                    ui.add(egui::Label::new(dim(p, line2)).truncate());
                });
            }
            Loaded::First => {}
        }
    }

    // ---- Posts tab --------------------------------------------------------------------------------

    pub(crate) fn posts_tab(&mut self, ui: &mut Ui, p: &Pal) {
        let Some(saved) = self.model.saved.clone() else {
            ui.add_space(S5);
            ui.vertical_centered(|ui| ui.label(dim(p, "The post list appears when a profile is open.")));
            return;
        };
        let now = ls::unix_now();
        let narrow = ui.available_width() < WIDE;
        let rec = &saved.record;
        let gone_all = saved.count(PostFilter::Gone);
        let anchor = match rec.last_change {
            Some(c) => {
                let when = ls::last_scanned(c.at, now, off);
                if narrow {
                    format!("Labels from {when}")
                } else if c.gone_posts == gone_all {
                    format!(
                        "Labels from the update on {when}: {} new, {} updated · {} gone",
                        comma(c.new_posts),
                        comma(c.updated_posts),
                        comma(c.gone_posts)
                    )
                } else {
                    format!(
                        "Labels from the update on {when}: {} new, {} updated · {} gone ({} in all)",
                        comma(c.new_posts),
                        comma(c.updated_posts),
                        comma(c.gone_posts),
                        comma(gone_all)
                    )
                }
            }
            None if gone_all > 0 && !narrow => {
                format!("{} gone from site. New and Updated appear after your next update.", comma(gone_all))
            }
            None => "No labels yet. New, Updated and Gone from site appear after your next update.".to_string(),
        };
        let chips = [
            (PostFilter::All, "All", saved.posts.len()),
            (PostFilter::New, "New", saved.count(PostFilter::New)),
            (PostFilter::Updated, "Updated", saved.count(PostFilter::Updated)),
            (PostFilter::Gone, "Gone", gone_all),
        ];
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            ui.spacing_mut().item_spacing.x = S1;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = S1;
                if narrow {
                    let cur = chips
                        .iter()
                        .find(|c| c.0 == self.model.post_filter)
                        .map_or_else(String::new, |c| format!("{} {}", c.1, comma(c.2)));
                    egui::ComboBox::from_id_salt("post_filter").selected_text(format!("Show: {cur}")).show_ui(
                        ui,
                        |ui| {
                            for (f, l, n) in chips {
                                ui.selectable_value(&mut self.model.post_filter, f, format!("{l} {}", comma(n)));
                            }
                        },
                    );
                } else {
                    for (f, l, n) in chips.iter().rev() {
                        let mut t = RichText::new(format!("{l} {}", comma(*n))).size(T_SMALL);
                        if *n == 0 && *f != PostFilter::All {
                            t = t.color(p.ink_dim);
                        }
                        ui.selectable_value(&mut self.model.post_filter, *f, t);
                    }
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(dim(p, anchor)).truncate());
                });
            });
        });
        ui.add_space(S1);

        let w = ui.available_width();
        let widths = vec![148.0, 96.0, (w - 148.0 - 96.0 - 72.0 - 14.0).max(80.0), 72.0];
        let head = Cols::new(ui, 28.0, widths.clone());
        ui.painter().rect_filled(head.rect(), CornerRadius { nw: RADIUS, ne: RADIUS, sw: 0, se: 0 }, p.sunken);
        for (i, (t, right)) in
            [("Label", false), ("Date", false), ("Post", false), ("Files", true)].into_iter().enumerate()
        {
            head.put(ui, i, right, |ui| {
                ui.label(dim(p, t));
            });
        }
        let shown: Vec<usize> =
            (0..saved.posts.len()).filter(|&i| self.model.post_filter.matches(saved.posts[i].label)).collect();
        let avail = ui.available_height();
        let change_at = rec.last_change.map(|c| c.at);
        Frame::new()
            .stroke(Stroke::new(1.0, p.line))
            .corner_radius(CornerRadius { nw: 0, ne: 0, sw: RADIUS, se: RADIUS })
            .show(ui, |ui| {
                ui.set_width(w - 2.0);
                ui.set_min_height((avail - 4.0).max(0.0));
                ui.spacing_mut().item_spacing.y = 0.0;
                if shown.is_empty() {
                    ui.add_space(S5);
                    ui.vertical_centered(|ui| ui.label(dim(p, "No posts match this filter.")));
                    return;
                }
                ScrollArea::vertical().auto_shrink([false, false]).max_height((avail - 4.0).max(60.0)).show_rows(
                    ui,
                    POST_ROW_H,
                    shown.len(),
                    |ui, range| {
                        for r in range {
                            let post = &saved.posts[shown[r]];
                            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), POST_ROW_H), Sense::hover());
                            if r % 2 == 1 {
                                ui.painter().rect_filled(rect, 0.0, p.surface);
                            }
                            let cols = Cols::at(rect, widths.clone());
                            let when = change_at.map(|t| ls::short_time(t, off)).unwrap_or_default();
                            cols.put(ui, 0, false, |ui| match post.label {
                                Some(PostLabel::New) => {
                                    pill(ui, "New", p.accent, &format!("First seen in the update on {when}."));
                                }
                                Some(PostLabel::Updated) => {
                                    pill(
                                        ui,
                                        "Updated",
                                        p.ink,
                                        &format!("Pawchive's copy of this post changed in the update on {when}. Files are only downloaded again if they are new."),
                                    );
                                }
                                Some(PostLabel::Gone) => {
                                    pill(
                                        ui,
                                        "× Gone from site",
                                        p.warn,
                                        "Pawchive no longer lists this post. Files you downloaded are still in your folder.",
                                    );
                                }
                                None => {}
                            });
                            cols.put(ui, 1, false, |ui| {
                                ui.label(num(ls::post_date(post.published.as_deref())).color(p.ink_dim));
                            });
                            cols.put(ui, 2, false, |ui| {
                                let t = RichText::new(&post.title);
                                let t = if post.label == Some(PostLabel::Gone) { t.color(p.ink_dim) } else { t };
                                ui.add(egui::Label::new(t).truncate());
                            });
                            cols.put(ui, 3, true, |ui| {
                                ui.label(num(comma(post.files)).color(p.ink_dim));
                            });
                        }
                    },
                );
            });
    }
}
