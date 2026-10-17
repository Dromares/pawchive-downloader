//! The four regions of the window (`ui-spec` section 2): title bar, left column (steps 1 to 3),
//! centre (status card, tabs, table) and the bottom action bar. Drawing only: clicks are returned
//! as [`Action`]s and handled in `App::perform`.

use std::time::Instant;

use eframe::egui::{
    self,
    text::{CCursor, CCursorRange},
    vec2, Align, Button, Color32, CornerRadius, Frame, Layout, Margin, ProgressBar, Rect, RichText, ScrollArea, Sense,
    Stroke, TextEdit, Ui,
};
use pawchive_core::classify::{Category, CustomExts};
use pawchive_core::external::ExtHost;
use pawchive_core::names::{creator_dir, post_dir};
use pawchive_core::plan::JobOrigin;

use crate::app::{Action, App, Filter, ReadyRow, Tab};
use crate::derive::{self, TYPE_ROWS};
use crate::format::{clock, comma, eta, files as files_word, hms_utc, human_size, last_component, post_label};
use crate::state::{explain_failure, explain_scan_error, Level, Model, Phase, Row, RowState, Sizing};
use crate::theme::*;
use crate::widgets::{bar, card, dim, heading, indeterminate, num, primary, readout, secondary, Cols};

const PRIMARY_W: f32 = 164.0;
const BROWSE_W: f32 = 92.0;
const SLOWDOWN_SECS: f32 = 60.0;

pub fn url_id() -> egui::Id {
    egui::Id::unique("profile_link")
}

/// Ctrl+L: focus the link field and select what is in it.
pub fn focus_url(ctx: &egui::Context, len: usize) {
    let id = url_id();
    ctx.memory_mut(|m| m.request_focus(id));
    if let Some(mut st) = TextEdit::load_state(ctx, id) {
        st.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(len))));
        st.store(ctx, id);
    }
}

fn filter_match(f: Filter, s: &RowState) -> bool {
    match f {
        Filter::All => true,
        Filter::Active => matches!(s, RowState::Active | RowState::Retrying { .. }),
        Filter::Queued => matches!(s, RowState::Queued | RowState::Stopped),
        Filter::Done => matches!(s, RowState::Done | RowState::Skipped),
        Filter::Failed => matches!(s, RowState::Failed { .. }),
    }
}

/// `≥ 200.3 MB` while some sizes are still unknown, else the plain size.
pub(crate) fn size_label(bytes: u64, partial: bool) -> String {
    if partial {
        format!("≥ {}", human_size(bytes))
    } else {
        human_size(bytes)
    }
}

impl App {
    // =========================================================================================
    // Title bar
    // =========================================================================================

    pub(crate) fn titlebar(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            ui.label(RichText::new("Pawchive Downloader").size(T_TITLE).strong());
            ui.label(dim(p, format!("v{}", pawchive_core::VERSION)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(Button::new("Settings").min_size(vec2(0.0, 30.0))).on_hover_text("Ctrl+,").clicked() {
                    acts.push(Action::OpenSettings);
                }
            });
        });
    }

    // =========================================================================================
    // Left column: the three inputs
    // =========================================================================================

    pub(crate) fn left(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        // Step 3 is pinned to the bottom so Browse… is always reachable and the steps never scroll off.
        // The panel must be laid out first to claim its space, but egui's Tab order is widget creation
        // order, so it only reserves a slot for Browse…; the button itself is created after step 2
        // (UI-SPEC §7: … Extensions → Browse… → tabs).
        let browse_slot = egui::Panel::bottom("save_to")
            .show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin { left: 0, right: 0, top: 8, bottom: 14 }))
            .show(ui, |ui| self.save_to(ui, p))
            .inner;
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = S2;
            self.step_profile(ui, p, acts);
            ui.add_space(S3);
            self.step_types(ui, p, acts);
        });
        let can = !self.model.phase.busy() && !self.picker_busy;
        let browse = Button::new("Browse…").min_size(vec2(BROWSE_W, 32.0));
        if ui.put(browse_slot, |ui: &mut Ui| ui.add_enabled(can, browse)).on_hover_text("Ctrl+O").clicked() {
            acts.push(Action::Browse);
        }
    }

    fn step_profile(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        // The heading row reserves a slot for `‹ Saved profiles`; the link itself is created after
        // Full rescan so the Tab order is URL, button, Full rescan, back link (ui-spec-library §3).
        let back_slot = ui
            .horizontal(|ui| {
                heading(ui, p, "1", "Profile link");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.allocate_exact_size(vec2(130.0, 22.0), Sense::hover()).0
                })
                .inner
            })
            .inner;
        let editable = self.url_editable();
        let scanning = self.model.phase == Phase::Scanning;
        let updating_link = self.updating_link();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = S2;
            let w = ui.available_width() - 76.0 - S2;
            let has_error = self.url_error.is_some();
            let resp = ui
                .scope(|ui| {
                    if has_error {
                        let wd = &mut ui.visuals_mut().widgets;
                        for ws in [&mut wd.inactive, &mut wd.hovered, &mut wd.active] {
                            ws.bg_stroke = Stroke::new(1.5, p.bad);
                        }
                    }
                    crate::urlmenu::field(ui, url_id(), &mut self.url, w, editable)
                })
                .inner;
            if resp.changed() {
                self.url_error = None;
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                acts.push(Action::Scan);
            }
            if scanning {
                if secondary(ui, "Cancel", 76.0, true).clicked() {
                    acts.push(Action::CancelScan);
                }
            } else {
                let label = if updating_link { "Update" } else { "Scan" };
                let b = ui.add_enabled(
                    editable && !self.url.trim().is_empty(),
                    Button::new(RichText::new(label).strong()).min_size(vec2(76.0, 32.0)),
                );
                let b = if updating_link {
                    b.on_hover_text(
                        "Checks whether the profile changed (1 request). If it did, reads the whole post list. Ctrl+U",
                    )
                } else {
                    b
                };
                if b.clicked() {
                    acts.push(Action::Scan);
                }
            }
        });
        if let Some(e) = &self.url_error {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("⚠").color(p.bad));
                ui.label(RichText::new(e).color(p.bad).size(T_SMALL));
            });
        } else if self.saved_hint(ui, p, acts) {
            // the library's lines: the open profile with Full rescan, or "already in your library"
        } else if let (Some(c), false) = (&self.model.creator, self.model.phase == Phase::Empty) {
            let head = format!("{} · user {}", c.service.as_str(), c.id);
            let post_id = self.model.scan_target.as_ref().and_then(|t| t.post_id.clone());
            let line = match (&post_id, self.model.phase) {
                (Some(id), Phase::Scanning) => format!("{head} · post {id} · reading…"),
                (Some(id), Phase::ScanError) => format!("{head} · post {id} · scan incomplete"),
                (Some(id), _) => format!("{head} · post {id}"),
                (None, Phase::Scanning) => format!("{head} · scanning…"),
                (None, Phase::ScanError) => format!("{head} · scan incomplete"),
                (None, _) => format!("{head} · {} posts", comma(self.model.posts)),
            };
            ui.label(dim(p, line));
            if let Some(id) = post_id {
                // The link is the user's statement of scope: say back what was understood.
                ui.add(
                    egui::Label::new(dim(
                        p,
                        format!(
                            "Only this post is scanned. Remove \u{201c}/post/{id}\u{201d} from the link to scan the whole creator."
                        ),
                    ))
                    .wrap(),
                );
            }
        } else {
            ui.label(dim(p, "Paste a link, then press Scan (Enter)."));
        }
        if self.model.library_on() && self.model.saved.is_some() && self.model.phase != Phase::Empty {
            let can = !self.model.phase.busy();
            // `new_child`, not `put`: `put` would move the parent's cursor back up to the slot, and
            // step 2 would be drawn over step 1.
            let mut slot =
                ui.new_child(egui::UiBuilder::new().max_rect(back_slot).layout(Layout::right_to_left(Align::Center)));
            let r =
                crate::library_view::back_link(&mut slot, p, can, "Wait for the current run to finish, or cancel it.");
            if r.clicked() {
                acts.push(Action::BackToList);
            }
        }
    }

    fn step_types(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        heading(ui, p, "2", "What to download");
        let phase = self.model.phase;
        // An update keeps the saved counts on screen; only a first scan has nothing to show yet.
        let updating = self.model.update.is_some();
        let live = phase == Phase::Scanning && !updating;
        if !live
            && !updating
            && !matches!(phase, Phase::Ready | Phase::Cancelled | Phase::Finished | Phase::Paused | Phase::Downloading)
        {
            ui.label(dim(p, "File types appear here after the scan."));
            return;
        }
        let summary = self.summary();
        let editable = phase == Phase::Ready;
        let w = ui.available_width();
        let widths = vec![100.0, (w - 100.0 - 40.0 - 84.0).max(40.0), 40.0, 84.0];
        let probing = self.sizing_running();
        ui.spacing_mut().item_spacing.y = 0.0;

        let head = Cols::new(ui, 20.0, widths.clone());
        head.put(ui, 0, false, |ui| {
            ui.label(dim(p, "Type"));
        });
        head.put(ui, 2, true, |ui| {
            ui.label(dim(p, "Files"));
        });
        head.put(ui, 3, true, |ui| {
            ui.label(dim(p, "Size"));
        });
        for (cat, exts) in TYPE_ROWS {
            let stat = summary.per_category.get(&cat).cloned().unwrap_or_default();
            let has = stat.count > 0;
            let mut on = self.checked.contains(&cat) && has;
            let mut changed = false;
            let c = Cols::new(ui, ROW_H + 2.0, widths.clone());
            c.put(ui, 0, false, |ui| {
                ui.add_enabled_ui(editable && has, |ui| changed = ui.checkbox(&mut on, cat.label()).changed());
            });
            c.put(ui, 1, false, |ui| {
                ui.add(egui::Label::new(dim(p, exts).size(T_SMALL - 1.5)).truncate());
            });
            c.put(ui, 2, true, |ui| {
                // The session reports totals while scanning, not types, so counts wait for the end.
                let t = if live { "…".into() } else { comma(stat.count) };
                ui.label(num(t).color(if has || live { p.ink } else { p.ink_dim }));
            });
            c.put(ui, 3, true, |ui| size_cell(ui, p, live, has, stat.bytes, stat.unsized_files > 0, probing));
            if changed {
                if on {
                    self.checked.insert(cat);
                } else {
                    self.checked.remove(&cat);
                }
            }
        }
        ui.spacing_mut().item_spacing.y = S2;
        if let Sizing::Running { done, total } = self.model.sizing {
            ui.add_space(S1);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                ui.label(dim(p, format!("Measuring file sizes · {done} of {total}")));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(Button::new(RichText::new("Skip").size(T_SMALL)).small()).clicked() {
                        acts.push(Action::SkipSizing);
                    }
                });
            });
        }
        ui.add_space(S1);
        ui.separator();

        // Preview-only row: thumbnails of files Pawchive has no full-size copy of.
        let pc = summary.preview.count;
        let mut pv = self.include_preview && pc > 0;
        let mut changed = false;
        let c = Cols::new(ui, ROW_H + 2.0, vec![w - 40.0 - 84.0, 40.0, 84.0]);
        c.put(ui, 0, false, |ui| {
            ui.add_enabled_ui(editable && pc > 0, |ui| {
                changed = ui.checkbox(&mut pv, "Preview only (thumbnail)").changed();
            });
        });
        c.put(ui, 1, true, |ui| {
            let t = if live { "…".into() } else { comma(pc) };
            ui.label(num(t).color(if pc > 0 || live { p.ink } else { p.ink_dim }));
        });
        c.put(ui, 2, true, |ui| {
            ui.label(num("—").color(p.ink_dim));
        });
        if changed {
            self.include_preview = pv;
        }
        Frame::new().inner_margin(Margin { left: 26, right: 0, top: 0, bottom: 0 }).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(dim(
                    p,
                    "Pawchive doesn't hold the full-size files for these. Ticking this saves small thumbnails (photo.thumb.jpg), not the originals.",
                ))
                .wrap(),
            );
        });
        self.left_links_row(ui, p, editable, live);
        ui.add_space(S2);
        ui.spacing_mut().item_spacing.y = S2;
        ui.label(dim(p, "Also include these extensions"));
        ui.add_enabled(
            editable,
            TextEdit::singleline(&mut self.custom_ext)
                .hint_text("psd, mp4")
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8, 7)),
        );
        if !live && !CustomExts::parse(&self.custom_ext).is_empty() {
            let line = match summary.per_category.get(&Category::Custom) {
                Some(s) if s.count > 0 => {
                    format!("Matches {} · {}", files_word(s.count), size_label(s.bytes, s.unsized_files > 0))
                }
                _ => "No files with these extensions.".to_string(),
            };
            ui.label(dim(p, line));
        }
    }

    /// Draws step 3 and returns the rect left open for the Browse… button (see `left`).
    fn save_to(&mut self, ui: &mut Ui, p: &Pal) -> egui::Rect {
        ui.spacing_mut().item_spacing.y = S2;
        ui.separator();
        heading(ui, p, "3", "Save to");
        let slot = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = S2;
            let w = ui.available_width() - BROWSE_W - S2;
            if self.has_folder() {
                let path = self.output.trim().to_string();
                readout(ui, p, w, RichText::new(path.clone()).monospace(), Some(&path));
            } else {
                readout(ui, p, w, dim(p, "No folder chosen"), None);
            }
            ui.allocate_exact_size(vec2(BROWSE_W, 32.0), egui::Sense::hover()).0
        });
        let line = match (&self.model.creator, self.has_folder()) {
            (Some(c), true) => format!(
                "Files go in  …/{}/{}/",
                last_component(self.output.trim()),
                creator_dir(c.service.as_str(), &c.id)
            ),
            _ => "Each profile gets its own subfolder.".to_string(),
        };
        ui.add(egui::Label::new(dim(p, line)).truncate());
        slot.inner
    }

    // =========================================================================================
    // Centre: status card, tabs, table
    // =========================================================================================

    pub(crate) fn centre(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        if self.model.phase == Phase::Empty {
            if self.model.profiles().is_empty() {
                self.library_unavailable_card(ui, p);
                let first_run = self.model.library_on();
                empty_state(
                    ui,
                    p,
                    first_run,
                    if matches!(self.model.lib, crate::state::Lib::Unavailable(_)) { S4 } else { 64.0 },
                );
                if first_run {
                    ui.vertical_centered(|ui| self.first_run_card(ui, p));
                }
            } else {
                self.saved_list(ui, p, acts);
            }
            return;
        }
        let now = Instant::now();
        self.status_card(ui, p, now);
        ui.add_space(S3);
        let updating = self.model.update.is_some();
        if matches!(self.model.phase, Phase::Scanning | Phase::ScanError) && !updating {
            ui.add_space(S5);
            let msg = if self.model.phase == Phase::Scanning {
                "The file list appears when the scan finishes."
            } else {
                "Nothing to list yet: the scan didn't finish."
            };
            ui.vertical_centered(|ui| ui.label(dim(p, msg)));
            return;
        }
        self.tabs(ui, p);
        ui.add_space(S1);
        if self.tab == Tab::Files && self.run_rows_shown() {
            self.filter_row(ui);
            ui.add_space(S1);
        }
        match self.tab {
            Tab::Files => self.files_table(ui, p, now),
            Tab::Links => self.links_tab(ui, p, acts),
            Tab::Posts => self.posts_tab(ui, p),
            Tab::Problems => self.problems(ui, p, acts),
            Tab::Log => self.log_view(ui, p, acts),
        }
    }

    /// The Files table shows the run's rows (not the "what will be saved" preview).
    fn run_rows_shown(&self) -> bool {
        !matches!(self.model.phase, Phase::Ready) && self.model.update.is_none()
    }

    fn tabs(&mut self, ui: &mut Ui, p: &Pal) {
        let running = self.run_rows_shown();
        let files = if running { self.model.pawchive_rows().count() } else { self.ready_rows().len() };
        let n_links = self.model.links.len();
        let failed = self.model.failed();
        let problems = self.model.problem_count();
        // The Posts tab exists once a creator scan has finished and the library saved it.
        let posts = self.model.saved.as_ref().map(|s| s.posts.len());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = S1;
            ui.selectable_value(&mut self.tab, Tab::Files, format!("Files {}", comma(files)));
            // Always present once a scan has finished (the other tabs never shift); the app never switches to it by itself.
            let links_label =
                if n_links > 0 { format!("External links {}", comma(n_links)) } else { "External links".into() };
            ui.selectable_value(&mut self.tab, Tab::Links, links_label);
            if let Some(n) = posts {
                ui.selectable_value(&mut self.tab, Tab::Posts, format!("Posts {}", comma(n)));
            } else if self.tab == Tab::Posts {
                self.tab = Tab::Files;
            }
            let mut label =
                RichText::new(if problems > 0 { format!("Problems {problems}") } else { "Problems".to_string() });
            if failed > 0 {
                label = label.color(p.bad);
            } else if problems > 0 {
                label = label.color(p.warn);
            }
            ui.selectable_value(&mut self.tab, Tab::Problems, label);
            ui.selectable_value(&mut self.tab, Tab::Log, "Log");
        });
    }

    /// The Files chips (`All · Active · Queued · Done · Failed`) in a 28 pt row under the tab strip: with five
    /// tabs they no longer fit beside it. Below 640 pt they collapse into one dropdown (ui-spec-library §1.6).
    fn filter_row(&mut self, ui: &mut Ui) {
        let narrow = ui.available_width() < 640.0;
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            ui.spacing_mut().item_spacing.x = S1;
            let rows = &self.model.rows;
            let count = |f: Filter| rows.iter().filter(|r| r.is_pawchive() && filter_match(f, &r.state)).count();
            let all = [
                (Filter::All, "All"),
                (Filter::Active, "Active"),
                (Filter::Queued, "Queued"),
                (Filter::Done, "Done"),
                (Filter::Failed, "Failed"),
            ];
            if narrow {
                let cur = all
                    .iter()
                    .find(|(f, _)| *f == self.filter)
                    .map(|(f, l)| format!("{l} {}", count(*f)))
                    .unwrap_or_default();
                egui::ComboBox::from_id_salt("filter").selected_text(format!("Show: {cur}")).show_ui(ui, |ui| {
                    for (f, l) in all {
                        ui.selectable_value(&mut self.filter, f, format!("{l} {}", count(f)));
                    }
                });
            } else {
                for (f, l) in all {
                    let n = count(f);
                    if f != Filter::All && n == 0 && f != self.filter {
                        continue; // zero-count chips stay hidden unless selected
                    }
                    ui.selectable_value(&mut self.filter, f, RichText::new(format!("{l} {n}")).size(T_SMALL));
                }
            }
        });
    }

    // ---- status card -------------------------------------------------------------------------

    fn status_card(&mut self, ui: &mut Ui, p: &Pal, now: Instant) {
        if self.library_card(ui, p, now) {
            return;
        }
        let m = &self.model;
        let total = m.rows.len();
        let saved = m.saved();
        let counter = format!("{saved} / {total} files");
        match m.phase {
            Phase::Scanning => {
                let sp = m.scan_progress;
                let t = ui.input(|i| i.time) as f32 * 0.6 % 2.0;
                let one_post = m.scan_target.as_ref().is_some_and(|t| t.post_id.is_some());
                card(ui, p, p.accent, |ui| {
                    let head = if one_post { "Reading the post…" } else { "Scanning profile…" };
                    card_head(ui, p, Lead::Spinner, head, Some(clock(m.scan_elapsed(now))));
                    indeterminate(ui, p, if t > 1.0 { 2.0 - t } else { t });
                    if one_post {
                        ui.label(dim(p, "One request to Pawchive: only this post is scanned."));
                        return;
                    }
                    ui.horizontal(|ui| {
                        ui.label(num(format!("page {}", sp.pages.max(1))));
                        ui.label(dim(p, "·"));
                        ui.label(num(format!("{} posts", comma(sp.posts))));
                        ui.label(dim(p, "·"));
                        ui.label(num(format!("{} files found", comma(sp.files))));
                    });
                    if m.scan_stalled(now) {
                        ui.label(RichText::new("Still waiting for pawchive.pw…").color(p.warn).size(T_SMALL));
                    } else {
                        ui.label(dim(
                            p,
                            "Pawchive asks for about one request per second, so a big profile takes a few minutes.",
                        ));
                    }
                });
            }
            Phase::ScanError => {
                let f = m.scan_failure.as_ref();
                let page = f.map_or(1, |f| f.progress.pages + 1);
                let cause = f.map(|f| explain_scan_error(&f.message, m.posts)).unwrap_or_default();
                card(ui, p, p.bad, |ui| {
                    card_head(ui, p, Lead::Glyph("⚠", p.bad), &format!("Scan stopped at page {page}"), None);
                    ui.add(egui::Label::new(cause).truncate());
                    // The session cannot resume a scan from an offset, so the card says what Scan again does.
                    ui.label(dim(p, "Scan again starts over from the first page; the posts found so far aren't kept."));
                });
            }
            Phase::Ready => {
                let sel = self.selection();
                let summary = self.summary();
                let m = &self.model;
                let all = m.files.len();
                let n_links = m.links.len();
                let link_sel = crate::links_state::totals(&m.links).selected;
                let title = match &m.scan_post {
                    Some(post) => format!("Scan complete — post \u{201c}{}\u{201d}, {}", post.title, files_word(all)),
                    None if all == 0 => "Scan complete — no files found".to_string(),
                    None => format!("Scan complete — {} posts, {}", comma(m.posts), files_word(all)),
                };
                let one_post = m.scan_post.is_some();
                let first_saved = m.saved.is_some();
                let mut line = format!("{} selected · {}", files_word(sel.files), size_label(sel.bytes, sel.partial));
                if sel.thumbnails {
                    line.push_str(" + thumbnails (size not measured)");
                }
                let downloadable: usize = summary.per_category.values().map(|s| s.count).sum();
                let api_preview = summary.preview.count - summary.preview_missing;
                let gone = summary.missing;
                let note = derive::missing_note(&summary);
                let finished_unsized =
                    !self.sizes_are_lower_bounds() && sel.unsized_files > 0 && !self.sizing_running();
                // The card is green only while it has nothing to warn about.
                let stripe = if gone > 0 { p.warn } else { p.ok };
                card(ui, p, stripe, |ui| {
                    card_head(ui, p, Lead::Glyph(if gone > 0 { "⚠" } else { "✓" }, stripe), &title, None);
                    if all == 0 && n_links == 0 {
                        ui.label(if one_post {
                            "No files or links found in this post."
                        } else {
                            "This profile has no downloadable files, or the id doesn't exist."
                        });
                    } else {
                        ui.label(line);
                        // A 404 on the size probe is as permanent as a 404 on download.
                        for l in &note {
                            ui.add(egui::Label::new(RichText::new(l)).wrap());
                        }
                        if finished_unsized {
                            ui.label(dim(
                                p,
                                format!(
                                    "{} files couldn't be measured, so the size above is at least that.",
                                    comma(sel.unsized_files)
                                ),
                            ));
                        }
                        if api_preview > downloadable {
                            ui.horizontal_top(|ui| {
                                ui.label(RichText::new("ⓘ").color(p.accent));
                                ui.add(
                                    egui::Label::new(dim(
                                        p,
                                        format!(
                                            "Mostly preview-only: {} of {} files. Pawchive has full-size copies of just {}.",
                                            comma(api_preview),
                                            comma(all),
                                            comma(downloadable)
                                        ),
                                    ))
                                    .wrap(),
                                );
                            });
                        } else {
                            if n_links > 0 {
                                let plural = |n: usize, w: &str| {
                                    if n == 1 {
                                        format!("1 {w}")
                                    } else {
                                        format!("{} {w}s", comma(n))
                                    }
                                };
                                ui.add(
                                    egui::Label::new(dim(
                                        p,
                                        if link_sel == 0 {
                                            format!(
                                                "{} found, none selected. Open the External links tab to choose.",
                                                plural(n_links, "external link")
                                            )
                                        } else {
                                            format!(
                                                "Includes {} from external links. {} found in all.",
                                                files_word(link_sel),
                                                plural(n_links, "link")
                                            )
                                        },
                                    ))
                                    .wrap(),
                                );
                            }
                            if first_saved {
                                ui.add(
                                    egui::Label::new(dim(
                                        p,
                                        "Saved to your library. Next time, open it from Saved profiles and press Update.",
                                    ))
                                    .truncate(),
                                );
                            } else if gone == 0 && n_links == 0 {
                                ui.label(dim(
                                    p,
                                    "Everything listed below will be saved. Untick types on the left to leave them out.",
                                ));
                            }
                        }
                    }
                });
            }
            Phase::Downloading if m.slow_secs(now).is_some() => {
                let s = m.slow_secs(now).unwrap_or(0);
                card(ui, p, p.warn, |ui| {
                    card_head(
                        ui,
                        p,
                        Lead::Glyph("⚠", p.warn),
                        "The site asked us to slow down",
                        Some(format!("resuming in {s} s")),
                    );
                    bar(ui, 1.0 - s as f32 / SLOWDOWN_SECS, p.warn);
                    ui.label(format!(
                        "All downloads are paused for 60 s, then continue by themselves. {saved} of {total} files saved so far."
                    ));
                    ui.label(dim(p, "Nothing is lost: partial files are kept and resume where they stopped."));
                });
            }
            Phase::Downloading => {
                let mut parts = Vec::new();
                match m.bytes_total() {
                    Some(t) => parts.push(format!("{} of {}", human_size(m.bytes_done()), human_size(t))),
                    None => parts.push(format!("{} saved", human_size(m.bytes_done()))),
                }
                parts.push(m.speed().map_or("— MB/s".to_string(), |s| format!("{}/s", human_size(s as u64))));
                parts.push(format!("{} active", m.active()));
                if let Some(e) = m.eta_secs(now) {
                    parts.push(eta(e));
                }
                let folder = self.folder_hint();
                card(ui, p, p.accent, |ui| {
                    card_head(ui, p, Lead::Spinner, "Downloading", Some(counter));
                    bar(ui, m.fraction(), p.accent);
                    ui.label(parts.join(" · "));
                    let incl = match m.link_file_rows() {
                        0 => String::new(),
                        k => format!("incl. {} from external links · ", comma(k)),
                    };
                    ui.label(dim(p, format!("{incl}Elapsed {} · saving to {folder}", clock(m.elapsed(now)))));
                });
            }
            Phase::Paused => {
                let partial = m.partial();
                card(ui, p, p.warn, |ui| {
                    card_head(ui, p, Lead::Icon(Glyph::Pause, p.warn), "Paused", Some(counter));
                    bar(ui, m.fraction(), p.warn);
                    ui.label(match partial {
                        0 => format!("{} saved.", human_size(m.bytes_saved())),
                        1 => format!("{} saved. 1 partial file is kept.", human_size(m.bytes_saved())),
                        n => format!("{} saved. {n} partial files are kept.", human_size(m.bytes_saved())),
                    });
                    ui.label(dim(p, "Resume continues where each file stopped. Nothing is re-downloaded."));
                });
            }
            Phase::Cancelled => {
                let partial = m.partial();
                let unstarted = m.remaining().saturating_sub(partial);
                card(ui, p, p.line_strong, |ui| {
                    card_head(ui, p, Lead::Glyph("■", p.ink_dim), "Cancelled", Some(counter));
                    bar(ui, m.fraction(), p.line_strong);
                    ui.label(format!(
                        "{saved} files saved. {partial} partial {} kept; {unstarted} not started.",
                        if partial == 1 { "file was" } else { "files were" }
                    ));
                    ui.label(dim(p, "“Download remaining” picks up from here."));
                });
            }
            Phase::Finished if m.failed() > 0 => {
                let failed = m.failed();
                let title = format!("Finished with {}", derive::problems(failed));
                card(ui, p, p.warn, |ui| {
                    card_head(ui, p, Lead::Glyph("⚠", p.warn), &title, Some(counter));
                    bar(ui, saved as f32 / total.max(1) as f32, p.warn);
                    ui.label(format!(
                        "{saved} files saved ({}) in {}. {failed} failed.{}",
                        human_size(m.bytes_saved()),
                        clock(m.elapsed(now)),
                        derive::missing_sentence(m.missing())
                    ));
                    ui.label(dim(p, "See the Problems tab. Retry failed tries the ones that can still work."));
                });
            }
            Phase::Finished => {
                let folder = self.folder_hint();
                card(ui, p, p.ok, |ui| {
                    card_head(ui, p, Lead::Glyph("✓", p.ok), "Finished — everything was saved", Some(counter));
                    bar(ui, 1.0, p.ok);
                    ui.label(derive::finished_line(
                        m.saved() - m.skipped(),
                        &human_size(m.bytes_downloaded()),
                        m.skipped(),
                        &clock(m.elapsed(now)),
                    ));
                    ui.add(egui::Label::new(dim(p, folder)).truncate());
                });
            }
            Phase::Empty => {}
        }
    }

    /// `…/fanbox_1386751/` for card text.
    fn folder_hint(&self) -> String {
        match &self.model.creator {
            Some(c) => format!("…/{}/", creator_dir(c.service.as_str(), &c.id)),
            None => String::new(),
        }
    }

    // ---- files table -----------------------------------------------------------------------------

    fn files_table(&mut self, ui: &mut Ui, p: &Pal, now: Instant) {
        if self.model.phase == Phase::Ready || self.model.update.is_some() {
            let rows = self.ready_rows();
            table(ui, p, true, rows.len(), |ui, cols, r, zebra| ready_row(ui, p, cols, &rows[r], zebra));
            return;
        }
        let slow = self.model.slow_until.is_some_and(|u| u > now);
        let phase = self.model.phase;
        let rows = &self.model.rows;
        let mut idx: Vec<usize> =
            (0..rows.len()).filter(|&i| rows[i].is_pawchive() && filter_match(self.filter, &rows[i].state)).collect();
        // Live work first, then failures, stopped, queued; finished files sink (stable sort).
        idx.sort_by_key(|&i| match rows[i].state {
            RowState::Active | RowState::Retrying { .. } => 0,
            RowState::Failed { .. } => 1,
            RowState::Stopped => 2,
            RowState::Queued => 3,
            RowState::Done | RowState::Skipped => 4,
        });
        table(ui, p, false, idx.len(), |ui, cols, r, zebra| {
            run_row(ui, p, cols, &rows[idx[r]], phase, slow, now, zebra)
        });
    }

    // ---- problems tab ----------------------------------------------------------------------------

    fn problems(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        let rows = &self.model.rows;
        let bad: Vec<usize> = (0..rows.len()).filter(|&i| matches!(rows[i].state, RowState::Failed { .. })).collect();
        // Files the size probe got a 404 for: listed under one heading, never retried.
        let gone: Vec<usize> = (0..self.model.files.len()).filter(|&i| self.model.files[i].missing).collect();
        if bad.is_empty() && gone.is_empty() {
            ui.add_space(S5);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No problems").strong());
                ui.label(dim(p, "Files that fail after all retries are listed here, with the reason."));
            });
            return;
        }
        let retryable = self.model.retryable();
        let can_retry = matches!(self.model.phase, Phase::Finished | Phase::Cancelled);
        if !bad.is_empty() {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if bad.len() == 1 {
                        "1 file failed".to_string()
                    } else {
                        format!("{} files failed", bad.len())
                    })
                    .strong(),
                );
                ui.label(dim(p, format!("· {retryable} can be retried{}", derive::missing_header_part(gone.len()))));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let b = Button::new(format!("Retry failed ({retryable})")).min_size(vec2(0.0, 30.0));
                    if ui.add_enabled(retryable > 0 && can_retry, b).clicked() {
                        acts.push(Action::RetryFailed);
                    }
                });
            });
            ui.add_space(S1);
        }
        // One list, one row height: failed downloads, then the heading, then the missing files.
        let head_at = bad.len();
        let total = bad.len() + if gone.is_empty() { 0 } else { 1 + gone.len() };
        Frame::new().stroke(Stroke::new(1.0, p.line)).corner_radius(CornerRadius::same(RADIUS)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let h = ui.available_height().max(120.0);
            ScrollArea::vertical().auto_shrink([false, false]).max_height(h).show_rows(ui, 52.0, total, |ui, range| {
                for n in range {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::hover());
                    if n % 2 == 1 {
                        ui.painter().rect_filled(rect.shrink(1.0), 0.0, p.surface);
                    }
                    let inner = rect.shrink2(vec2(12.0, 6.0));
                    if n == head_at && !gone.is_empty() {
                        ui.scope_builder(
                            egui::UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)),
                            |ui| {
                                ui.label(RichText::new("⚠").color(p.warn).strong());
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(format!("Not on Pawchive · {}", comma(gone.len()))).strong(),
                                        )
                                        .truncate(),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(
                                                "Pawchive no longer has these files. Retry failed skips them.",
                                            )
                                            .color(p.ink_dim)
                                            .size(T_SMALL),
                                        )
                                        .truncate(),
                                    );
                                });
                            },
                        );
                        continue;
                    }
                    if n > head_at && !gone.is_empty() {
                        let f = &self.model.files[gone[n - head_at - 1]];
                        let post = post_dir(f.post_published.as_deref(), &f.post_id, &f.post_title);
                        ui.scope_builder(
                            egui::UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)),
                            |ui| {
                                ui.label(RichText::new("×").color(p.warn).strong());
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.add(egui::Label::new(RichText::new(&f.name).strong()).truncate());
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(format!("Not on Pawchive (404) · {post}"))
                                                .color(p.ink_dim)
                                                .size(T_SMALL),
                                        )
                                        .truncate(),
                                    );
                                });
                            },
                        );
                        continue;
                    }
                    let i = bad[n];
                    let r = &self.model.rows[i];
                    let RowState::Failed { error, permanent } = &r.state else { continue };
                    let right = Rect::from_min_max(egui::pos2(inner.right() - 84.0, inner.top()), inner.right_bottom());
                    let left = Rect::from_min_max(inner.left_top(), egui::pos2(inner.right() - 92.0, inner.bottom()));
                    ui.scope_builder(
                        egui::UiBuilder::new().max_rect(left).layout(Layout::left_to_right(Align::Center)),
                        |ui| {
                            ui.label(RichText::new("×").color(p.bad).strong());
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.add(egui::Label::new(RichText::new(&r.name).strong()).truncate());
                                let why = match r.origin {
                                    JobOrigin::Link { link, .. } => {
                                        let host =
                                            self.model.links.get(link).map(|l| l.link.host.name()).unwrap_or_default();
                                        format!("{} · {host} · {}", explain_failure(error), post_label(&r.post))
                                    }
                                    JobOrigin::Pawchive => format!("{} · {}", explain_failure(error), r.post),
                                };
                                ui.add(egui::Label::new(RichText::new(why).color(p.ink_dim).size(T_SMALL)).truncate());
                            });
                        },
                    );
                    ui.scope_builder(
                        egui::UiBuilder::new().max_rect(right).layout(Layout::right_to_left(Align::Center)),
                        |ui| {
                            if *permanent {
                                ui.label(dim(p, "Won't retry"));
                            } else if ui
                                .add_enabled(can_retry, Button::new("Retry").min_size(vec2(64.0, 28.0)))
                                .clicked()
                            {
                                acts.push(Action::RetryOne(i));
                            }
                        },
                    );
                }
            });
        });
    }

    // ---- log tab ---------------------------------------------------------------------------------

    fn log_view(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        let log = &self.model.log;
        ui.horizontal(|ui| {
            ui.label(dim(p, format!("{} entries · newest at the bottom", log.len())));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(Button::new("Copy log").small()).clicked() {
                    acts.push(Action::CopyLog);
                }
            });
        });
        let h = ui.available_height();
        Frame::new()
            .fill(p.sunken)
            .stroke(Stroke::new(1.0, p.line))
            .corner_radius(CornerRadius::same(RADIUS))
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 3.0;
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height((h - 24.0).max(60.0))
                    .stick_to_bottom(true)
                    .show_rows(ui, row_h, log.len(), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        for l in log.iter().skip(range.start).take(range.len()) {
                            ui.horizontal(|ui| {
                                ui.label(num(hms_utc(l.unix)).color(p.ink_dim));
                                let (g, c) = match l.level {
                                    Level::Info => ("·", p.ink_dim),
                                    Level::Warn => ("⚠", p.warn),
                                    Level::Error => ("×", p.bad),
                                };
                                ui.label(RichText::new(g).color(c));
                                ui.add(egui::Label::new(RichText::new(&l.text).monospace().size(T_MONO)).truncate());
                            });
                        }
                    });
            });
    }

    // =========================================================================================
    // Bottom action bar: one primary slot, always bottom-right
    // =========================================================================================

    pub(crate) fn action_bar(&mut self, ui: &mut Ui, p: &Pal, acts: &mut Vec<Action>) {
        let sel = self.selection();
        let phase = self.model.phase;
        let block = derive::download_block(self.has_folder(), sel.files, self.all_gone());
        let retryable = self.model.retryable();
        let remaining = self.model.remaining();
        let folder_exists =
            self.run_folder().is_some_and(|f| f.exists()) || (self.has_folder() && matches!(phase, Phase::Finished));
        let two_buttons = phase == Phase::Ready && self.model.saved.as_ref().is_some_and(|s| s.has_history());
        let new_only = if two_buttons { self.new_only() } else { Default::default() };
        let m = &self.model;
        ui.horizontal(|ui| {
            ui.set_min_height(BTN_H);
            let left_w = (ui.available_width() - 380.0).max(120.0);
            ui.allocate_ui_with_layout(vec2(left_w, BTN_H), Layout::left_to_right(Align::Center), |ui| {
                let t: RichText = match phase {
                    Phase::Empty => dim(p, "Paste a profile link to begin."),
                    Phase::Scanning if m.update.is_some() => {
                        dim(p, "Updating. Download unlocks when it finishes.").size(T_BODY)
                    }
                    Phase::Scanning => dim(p, "Scanning. Download unlocks when it finishes."),
                    Phase::ScanError => dim(p, "Nothing was saved. Scan again starts over."),
                    Phase::Ready | Phase::Cancelled if m.awaiting_checks => {
                        dim(p, awaiting_text(m, Instant::now())).size(T_BODY)
                    }
                    Phase::Ready if two_buttons => match (block, new_only.files) {
                        (Some(e), _) => dim(p, e).size(T_BODY),
                        (None, 0) => dim(p, "Nothing new to download.").size(T_BODY),
                        (None, n) => RichText::new(format!(
                            "{} · {}",
                            if n == 1 { "1 new file".to_string() } else { format!("{} new files", comma(n)) },
                            self.size_text(new_only.bytes, new_only.unsized_files, new_only.partial)
                        ))
                        .monospace(),
                    },
                    Phase::Ready => match block {
                        Some(e) => dim(p, e).size(T_BODY),
                        None => {
                            let mut s = format!(
                                "{} · {}",
                                files_word(sel.files),
                                self.size_text(sel.bytes, sel.unsized_files, sel.partial)
                            );
                            if sel.thumbnails {
                                s.push_str(" + thumbnails");
                            }
                            RichText::new(s).monospace()
                        }
                    },
                    Phase::Downloading if m.slow_until.is_some_and(|u| u > Instant::now()) => {
                        dim(p, "Waiting out the site's cool-down.").size(T_BODY)
                    }
                    Phase::Downloading => {
                        let speed = m.speed().map_or("— MB/s".to_string(), |s| format!("{}/s", human_size(s as u64)));
                        dim(p, format!("{} active · {speed}", m.active())).size(T_BODY)
                    }
                    Phase::Paused => dim(p, "Paused. Resume continues where each file stopped.").size(T_BODY),
                    Phase::Cancelled => dim(p, "Partial files were kept.").size(T_BODY),
                    Phase::Finished => dim(p, format!("Saved to {}", self.output.trim())).size(T_BODY),
                };
                ui.add(egui::Label::new(t).truncate());
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = S2;
                match phase {
                    Phase::Empty => {
                        primary(ui, p, "Download", PRIMARY_W, false).on_disabled_hover_text("Scan a profile first.");
                    }
                    Phase::Scanning => {
                        if secondary(ui, "Cancel scan", PRIMARY_W, true).clicked() {
                            acts.push(Action::CancelScan);
                        }
                    }
                    Phase::ScanError => {
                        if primary(ui, p, "Scan again", PRIMARY_W, true).clicked() {
                            acts.push(Action::Scan);
                        }
                    }
                    Phase::Ready | Phase::Cancelled if m.awaiting_checks => {
                        if secondary(ui, "Cancel", 92.0, true).clicked() {
                            acts.push(Action::Cancel);
                        }
                    }
                    Phase::Ready if two_buttons => {
                        // The primary is always bottom-right: "Download new only" once there is history.
                        let n = new_only.files;
                        let enabled = block.is_none() && n > 0;
                        let r = primary(ui, p, &format!("Download new only ({})", comma(n)), 224.0, enabled);
                        match (block, n) {
                            (Some(e), _) => {
                                r.on_disabled_hover_text(e);
                            }
                            (None, 0) => {
                                r.on_disabled_hover_text("Nothing new to download.");
                            }
                            (None, _) => {
                                if r.on_hover_text(
                                    "Saves only files you haven't downloaded before, even if you've moved or deleted older ones. Ctrl+Enter",
                                )
                                .clicked()
                                {
                                    acts.push(Action::DownloadNew);
                                }
                            }
                        }
                        let all = secondary(ui, &format!("Download all ({})", comma(sel.files)), 150.0, block.is_none());
                        match block {
                            Some(e) => {
                                all.on_disabled_hover_text(e);
                            }
                            None => {
                                if all
                                    .on_hover_text("Saves every selected file again, skipping files already in the folder. Use this to fill gaps.")
                                    .clicked()
                                {
                                    acts.push(Action::Download);
                                }
                            }
                        }
                    }
                    Phase::Ready => {
                        let r = primary(ui, p, &format!("Download {}", comma(sel.files)), PRIMARY_W, block.is_none());
                        match block {
                            Some(e) => {
                                r.on_disabled_hover_text(e);
                            }
                            None => {
                                if r.on_hover_text("Ctrl+Enter").clicked() {
                                    acts.push(Action::Download);
                                }
                            }
                        }
                    }
                    Phase::Downloading | Phase::Paused => {
                        let (label, act) =
                            if phase == Phase::Paused { ("Resume", Action::Resume) } else { ("Pause", Action::Pause) };
                        if primary(ui, p, label, PRIMARY_W, true).on_hover_text("Ctrl+P").clicked() {
                            acts.push(act);
                        }
                        if secondary(ui, "Cancel", 92.0, true).clicked() {
                            acts.push(Action::Cancel);
                        }
                    }
                    Phase::Cancelled => {
                        if primary(ui, p, &format!("Download remaining ({remaining})"), 200.0, remaining > 0).clicked()
                        {
                            acts.push(Action::Download);
                        }
                    }
                    Phase::Finished if retryable > 0 => {
                        if primary(ui, p, &format!("Retry failed ({retryable})"), PRIMARY_W, true).clicked() {
                            acts.push(Action::RetryFailed);
                        }
                        if secondary(ui, "Open folder", 112.0, folder_exists).clicked() {
                            acts.push(Action::OpenFolder);
                        }
                    }
                    Phase::Finished => {
                        if primary(ui, p, "Open folder", PRIMARY_W, folder_exists).clicked() {
                            acts.push(Action::OpenFolder);
                        }
                        if secondary(ui, "Copy path", 100.0, true).clicked() {
                            acts.push(Action::CopyPath);
                        }
                    }
                }
            });
        });
    }
}

// ---- free drawing helpers ---------------------------------------------------------------------

/// Bottom-bar line while Download waits for its picked links to be checked.
fn awaiting_text(m: &Model, now: Instant) -> String {
    let mut s = "Checking links before downloading".to_string();
    if let Some((done, total)) = m.checking {
        s.push_str(&format!(" · {done} of {total}"));
    }
    let cooling = [ExtHost::Drive, ExtHost::Mediafire]
        .into_iter()
        .find_map(|h| m.host_cooldown_secs(h, now).map(|secs| (h, secs)));
    if let Some((host, secs)) = cooling {
        s.push_str(&format!(" · {} asked us to slow down, resuming in {secs} s", host.name()));
    }
    s
}

fn size_cell(ui: &mut Ui, p: &Pal, live: bool, has: bool, bytes: u64, partial: bool, probing: bool) {
    if live {
        ui.label(num("…").color(p.ink_dim));
    } else if !has {
        ui.label(num("—").color(p.ink_dim));
    } else if partial {
        // Some sizes are unknown: a lower bound in the dim colour; nothing known yet reads "…"
        // while probing and "—" once probing has ended.
        let t = match (bytes, probing) {
            (0, true) => "…".to_string(),
            (0, false) => "—".to_string(),
            _ => size_label(bytes, true),
        };
        ui.label(num(t).color(p.ink_dim));
    } else {
        ui.label(num(human_size(bytes)));
    }
}

pub(crate) enum Lead {
    Spinner,
    Glyph(&'static str, Color32),
    Icon(Glyph, Color32),
}

/// Headline row of a status card: spinner or glyph, 16 pt strong title, counter on the right.
pub(crate) fn card_head(ui: &mut Ui, p: &Pal, lead: Lead, title: &str, right: Option<String>) {
    ui.horizontal(|ui| {
        match lead {
            Lead::Spinner => {
                ui.add(egui::Spinner::new().size(18.0).color(p.accent));
            }
            Lead::Glyph(g, c) => {
                ui.label(RichText::new(g).color(c).size(T_CARD));
            }
            Lead::Icon(g, c) => draw_icon(ui, &g, c, T_CARD),
        }
        if right.is_none() {
            // A long post title truncates at the end; the tooltip carries all of it.
            ui.add(egui::Label::new(RichText::new(title).size(T_CARD).strong()).truncate());
        } else {
            ui.label(RichText::new(title).size(T_CARD).strong());
        }
        if let Some(r) = right {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(num(r).color(p.ink_dim));
            });
        }
    });
}

fn empty_state(ui: &mut Ui, p: &Pal, _first_run: bool, top: f32) {
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("Download a creator's files from Pawchive").size(T_TITLE).strong());
        ui.add_space(S2);
        ui.label(
            RichText::new("Paste a profile link, pick the kinds of files you want, and choose where to save them.")
                .color(p.ink_dim),
        );
        ui.add_space(S5);
        Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.line))
            .corner_radius(CornerRadius::same(RADIUS + 2))
            .inner_margin(Margin::symmetric(20, 16))
            .show(ui, |ui| {
                ui.set_width(440.0);
                for (n, a, b) in [
                    ("1", "Scan", "Reads the creator's post list. About one page per second."),
                    ("2", "Choose", "Tick file types. Sizes fill in as they're measured."),
                    ("3", "Download", "Two at a time by default. Pause or cancel any time."),
                ] {
                    ui.horizontal(|ui| {
                        let (r, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, format!("Step {n}")));
                        ui.painter().circle_stroke(r.center(), 10.5, Stroke::new(1.0, p.line_strong));
                        ui.painter().text(
                            r.center(),
                            egui::Align2::CENTER_CENTER,
                            n,
                            egui::FontId::proportional(12.0),
                            p.ink_dim,
                        );
                        ui.label(RichText::new(a).strong());
                        ui.label(dim(p, b));
                    });
                    ui.add_space(2.0);
                }
            });
        ui.add_space(S4);
        ui.label(dim(p, "Link format:  https://pawchive.pw/{service}/user/{id}   (services: patreon, fanbox)"));
    });
}

/// Column layout of the files table; the Post column drops out below 600 pt.
struct TableCols {
    widths: Vec<f32>,
    show_post: bool,
}

impl TableCols {
    fn new(w: f32) -> Self {
        let show_post = w >= 620.0;
        // Size fits "999.9 MB / 999.9 MB" in the monospace face, with room to spare; Progress fits bar + "Stopped at 66%".
        let (stat, size, prog, post) = (30.0, 158.0, 150.0, if show_post { 132.0 } else { 0.0 });
        let name = (w - stat - size - prog - post - 14.0).max(120.0);
        TableCols { widths: vec![stat, name, post, prog, size], show_post }
    }
}

/// Header plus a virtualised body (`show_rows`), so 4,000+ rows cost one screenful per frame.
fn table(ui: &mut Ui, p: &Pal, ready: bool, count: usize, mut row: impl FnMut(&mut Ui, &Cols, usize, bool)) {
    let w = ui.available_width();
    let tc = TableCols::new(w);
    let head = Cols::new(ui, 28.0, tc.widths.clone());
    ui.painter().rect_filled(head.rect(), CornerRadius { nw: RADIUS, ne: RADIUS, sw: 0, se: 0 }, p.sunken);
    head.put(ui, 1, false, |ui| {
        ui.label(dim(p, "File"));
    });
    if tc.show_post {
        head.put(ui, 2, false, |ui| {
            ui.label(dim(p, "Post"));
        });
    }
    head.put(ui, 3, false, |ui| {
        ui.label(dim(p, if ready { "Status" } else { "Progress" }));
    });
    head.put(ui, 4, true, |ui| {
        ui.label(dim(p, "Size"));
    });
    let avail = ui.available_height();
    Frame::new()
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(CornerRadius { nw: 0, ne: 0, sw: RADIUS, se: RADIUS })
        .show(ui, |ui| {
            ui.set_width(w - 2.0);
            ui.set_min_height((avail - 4.0).max(0.0));
            ui.spacing_mut().item_spacing.y = 0.0;
            ScrollArea::vertical().auto_shrink([false, false]).max_height((avail - 4.0).max(60.0)).show_rows(
                ui,
                ROW_H,
                count,
                |ui, range| {
                    for r in range {
                        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
                        let zebra = r % 2 == 1;
                        if zebra {
                            ui.painter().rect_filled(rect, 0.0, p.surface);
                        }
                        let cols = Cols::at(rect, tc.widths.clone());
                        row(ui, &cols, r, zebra);
                    }
                },
            );
        });
}

/// A status icon. Pause and retry are drawn with the painter: the bundled fonts have no glyph for
/// them (they showed as `▯`), and a drawn shape cannot go missing on someone else's machine.
#[derive(Clone, Copy)]
pub(crate) enum Glyph {
    Text(&'static str),
    Pause,
    Retry,
}

fn draw_icon(ui: &mut Ui, g: &Glyph, c: Color32, size: f32) {
    match g {
        Glyph::Text(t) => {
            ui.label(RichText::new(*t).color(c).size(size));
        }
        Glyph::Pause => {
            let (r, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
            let (bar, gap) = (size * 0.24, size * 0.18);
            let x0 = r.center().x - bar - gap / 2.0;
            for x in [x0, x0 + bar + gap] {
                let b = Rect::from_min_size(egui::pos2(x, r.top() + size * 0.14), vec2(bar, size * 0.72));
                ui.painter().rect_filled(b, CornerRadius::same(1), c);
            }
        }
        Glyph::Retry => {
            // A three-quarter circle with an arrow head: "again".
            let (r, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
            let (ctr, rad) = (r.center(), size * 0.36);
            let at = |a: f32| ctr + vec2(a.cos(), a.sin()) * rad;
            let (start, end) = (-0.3_f32, 4.4_f32);
            let pts: Vec<_> = (0..=20).map(|i| at(start + (end - start) * i as f32 / 20.0)).collect();
            let st = Stroke::new((size * 0.12).max(1.2), c);
            ui.painter().add(egui::Shape::line(pts, st));
            let tip = at(end);
            let dir = vec2((end + std::f32::consts::FRAC_PI_2).cos(), (end + std::f32::consts::FRAC_PI_2).sin());
            let side = vec2(-dir.y, dir.x);
            let h = size * 0.26;
            ui.painter().add(egui::Shape::convex_polygon(
                vec![tip + dir * h, tip - dir * (h * 0.2) + side * h * 0.8, tip - dir * (h * 0.2) - side * h * 0.8],
                c,
                Stroke::NONE,
            ));
        }
    }
}

fn glyph_cell(ui: &mut Ui, cols: &Cols, g: &Glyph, c: Color32) {
    cols.put(ui, 0, false, |ui| {
        ui.add_space(4.0);
        match g {
            Glyph::Text(t) => {
                ui.label(RichText::new(*t).color(c).strong());
            }
            other => draw_icon(ui, other, c, 14.0),
        }
    });
}

fn ready_row(ui: &mut Ui, p: &Pal, cols: &Cols, r: &ReadyRow, _zebra: bool) {
    if r.missing {
        glyph_cell(ui, cols, &Glyph::Text("×"), p.warn);
    } else {
        glyph_cell(ui, cols, &Glyph::Text("•"), p.ink_dim);
    }
    cols.put(ui, 1, false, |ui| {
        let t = RichText::new(&r.name);
        ui.add(egui::Label::new(if r.missing { t.color(p.ink_dim) } else { t }).truncate());
    });
    put_post(ui, p, cols, &r.post);
    cols.put(ui, 3, false, |ui| {
        if r.missing {
            ui.label(RichText::new("Not on Pawchive").size(T_SMALL).color(p.warn));
        } else if r.downloaded {
            let l = ui.label(dim(p, "✓ Downloaded"));
            if let Some(path) = &r.saved_path {
                l.on_hover_text(format!("Saved to {}", path.display()));
            }
        } else {
            ui.label(dim(p, "Ready"));
        }
    });
    cols.put(ui, 4, true, |ui| {
        ui.label(num(r.size.map_or("—".to_string(), human_size)).color(p.ink_dim));
    });
}

fn put_post(ui: &mut Ui, p: &Pal, cols: &Cols, post: &str) {
    if cols.rect().width() >= 620.0 {
        cols.put(ui, 2, false, |ui| {
            // The tooltip is the full folder name, not the elided "date · title" the cell shows.
            ui.add(egui::Label::new(dim(p, post_label(post))).truncate().show_tooltip_when_elided(false))
                .on_hover_text(post);
        });
    }
}

/// How one run row looks: glyph, status text and colours, optional mini bar.
struct RowLook {
    glyph: Glyph,
    color: Color32,
    text: String,
    bar: Option<(f32, Color32)>,
    sized: bool,
}

fn row_look(p: &Pal, r: &Row, phase: Phase, slow: bool, now: Instant) -> RowLook {
    let pct = r.percent();
    let pct_text = pct.map_or_else(|| human_size(r.done), |f| format!("{:.0}%", f * 100.0));
    let stopped = |p: &Pal| RowLook {
        glyph: Glyph::Pause,
        color: p.warn,
        text: pct.map_or("Stopped".into(), |f| format!("Stopped at {:.0}%", f * 100.0)),
        bar: pct.map(|f| (f, p.warn)),
        sized: true,
    };
    let waiting = |p: &Pal, sized: bool| RowLook {
        glyph: Glyph::Text("…"),
        color: p.warn,
        text: "Waiting".into(),
        bar: None,
        sized,
    };
    match &r.state {
        RowState::Queued => {
            RowLook { glyph: Glyph::Text("•"), color: p.ink_dim, text: "Queued".into(), bar: None, sized: false }
        }
        RowState::Active if slow => waiting(p, true),
        RowState::Active if phase == Phase::Paused => stopped(p),
        RowState::Active => RowLook {
            glyph: Glyph::Text("↓"),
            color: p.accent,
            text: pct_text,
            bar: pct.map(|f| (f, p.accent)),
            sized: true,
        },
        // During the site's cool-down nothing runs, whatever a row's own timer says; and a timer
        // that has run out is "about to go", not "retry in 0 s".
        RowState::Retrying { until, .. } if slow || *until <= now => waiting(p, false),
        RowState::Retrying { attempt, until } => {
            let secs = until.saturating_duration_since(now).as_secs_f64().ceil() as u64;
            RowLook {
                glyph: Glyph::Retry,
                color: p.warn,
                text: format!("Retry {attempt}/5 in {secs} s"),
                bar: None,
                sized: false,
            }
        }
        RowState::Done => {
            RowLook { glyph: Glyph::Text("✓"), color: p.ok, text: "Done".into(), bar: None, sized: false }
        }
        RowState::Skipped => {
            RowLook {
                glyph: Glyph::Text("✓"), color: p.ink_dim, text: "Already saved".into(), bar: None, sized: false
            }
        }
        RowState::Failed { .. } => {
            RowLook { glyph: Glyph::Text("×"), color: p.bad, text: "Failed".into(), bar: None, sized: false }
        }
        RowState::Stopped => stopped(p),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_row(ui: &mut Ui, p: &Pal, cols: &Cols, r: &Row, phase: Phase, slow: bool, now: Instant, _zebra: bool) {
    let look = row_look(p, r, phase, slow, now);
    glyph_cell(ui, cols, &look.glyph, look.color);
    cols.put(ui, 1, false, |ui| {
        let mut t = RichText::new(&r.name);
        if matches!(r.state, RowState::Queued) {
            t = t.color(p.ink_dim);
        }
        ui.add(egui::Label::new(t).truncate());
    });
    put_post(ui, p, cols, &r.post);
    cols.put(ui, 3, false, |ui| {
        if let Some((f, c)) = look.bar {
            let text = num(look.text.clone()).color(if look.color == p.accent { p.ink } else { p.ink_dim });
            // The bar gives way to the text, so the two can never reach into the Size column.
            let text_w = ui
                .painter()
                .layout_no_wrap(look.text.clone(), egui::TextStyle::Monospace.resolve(ui.style()), c)
                .size()
                .x;
            let bar_w = (ui.available_width() - text_w - ui.spacing().item_spacing.x - 2.0).clamp(20.0, 86.0);
            ui.scope(|ui| {
                ui.spacing_mut().interact_size.y = 6.0;
                ui.add(ProgressBar::new(f).desired_width(bar_w).fill(c).corner_radius(CornerRadius::same(3)));
            });
            ui.label(text);
        } else {
            let c = if matches!(look.color, c if c == p.ink_dim) { p.ink_dim } else { look.color };
            ui.label(RichText::new(look.text.clone()).color(c));
        }
    });
    cols.put(ui, 4, true, |ui| {
        let s = match (look.sized, r.total) {
            (true, Some(t)) => format!("{} / {}", human_size(r.done), human_size(t)),
            (true, None) => human_size(r.done),
            (false, Some(t)) => human_size(t),
            (false, None) if r.done > 0 => human_size(r.done),
            (false, None) => "—".into(),
        };
        ui.label(num(s).color(p.ink_dim));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn retrying(attempt: u32, until: Instant) -> Row {
        Row { state: RowState::Retrying { attempt, until }, ..Row::new("p", "f", None) }
    }

    /// DAN-1543 D4: the card says "resuming in 49 s", so a row must not say "Retry 1/5 in 0 s".
    #[test]
    fn rows_say_waiting_during_the_cool_down_and_when_their_timer_ran_out() {
        let p = Pal::new(crate::theme::Mode::Dark);
        let now = Instant::now();
        let cooling = row_look(&p, &retrying(1, now), Phase::Downloading, true, now);
        assert_eq!(cooling.text, "Waiting");
        let expired = row_look(&p, &retrying(1, now), Phase::Downloading, false, now + Duration::from_secs(1));
        assert_eq!(expired.text, "Waiting");
        let counting = row_look(&p, &retrying(2, now + Duration::from_secs(7)), Phase::Downloading, false, now);
        assert_eq!(counting.text, "Retry 2/5 in 7 s");
        assert!(matches!(counting.glyph, Glyph::Retry), "drawn, not a font glyph");
    }

    #[test]
    fn stopped_rows_use_the_drawn_pause_icon() {
        let p = Pal::new(crate::theme::Mode::Dark);
        let r = Row { state: RowState::Stopped, done: 5, ..Row::new("p", "f", Some(10)) };
        let look = row_look(&p, &r, Phase::Paused, false, Instant::now());
        assert_eq!(look.text, "Stopped at 50%");
        assert!(matches!(look.glyph, Glyph::Pause));
    }
}
