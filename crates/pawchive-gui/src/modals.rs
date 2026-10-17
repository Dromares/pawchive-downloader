//! The two dialogs (`ui-spec` sections 4 and 8): Settings and the quit confirmation.

use eframe::egui::{self, vec2, Align, Button, Layout, Margin, RichText, TextEdit};
use pawchive_core::settings::{MAX_CONCURRENCY, MIN_DELAY_MS};

use crate::app::{Action, App};
use crate::theme::*;
use crate::widgets::{dim, primary, readout, secondary};

impl App {
    pub(crate) fn settings_modal(&mut self, ctx: &egui::Context, p: &Pal) {
        let busy = self.model.phase.busy();
        let picking = self.picker_busy;
        let mut acts: Vec<Action> = Vec::new();
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("settings")).backdrop_color(p.backdrop()).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Settings").size(T_TITLE).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(dim(p, "Esc to close"));
                });
            });
            ui.add_space(S2);
            ui.separator();
            ui.add_space(S2);

            // One column: label, control, one line of help. No grid, so nothing can misalign.
            ui.label(RichText::new("Downloads at once").strong());
            ui.add(egui::Slider::new(&mut self.settings.concurrency, 1..=MAX_CONCURRENCY));
            ui.label(dim(p, "Pawchive's limit is 5. 2 is gentle and rarely trips its bot check."));
            ui.add_space(S3);

            ui.label(RichText::new("Pause between requests").strong());
            let mut secs = self.settings.delay_ms as f32 / 1000.0;
            let min_secs = MIN_DELAY_MS as f32 / 1000.0;
            if ui
                .add(egui::DragValue::new(&mut secs).range(min_secs..=10.0).speed(0.1).fixed_decimals(1).suffix(" s"))
                .changed()
            {
                self.settings.delay_ms = (secs * 1000.0).round() as u64;
            }
            ui.label(dim(p, "At least 1 s. Pawchive asks for no more than one request per second. Other sites get the same pause, each on its own clock."));
            ui.add_space(S3);

            ui.label(RichText::new("Contact (optional)").strong());
            ui.add_enabled(
                !busy,
                TextEdit::singleline(&mut self.settings.contact)
                    .hint_text("you@example.com")
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8, 7)),
            );
            ui.label(dim(p, "Sent to Pawchive in the User-Agent so it can reach you:"));
            ui.label(RichText::new(self.settings.clone().normalized().user_agent()).monospace().size(T_SMALL).color(p.ink_dim));
            ui.add_space(S3);

            ui.label(RichText::new("Cookie file (optional)").strong());
            ui.horizontal(|ui| {
                let w = ui.available_width() - 210.0;
                match &self.settings.cookie_file {
                    Some(path) => {
                        let s = path.display().to_string();
                        readout(ui, p, w, RichText::new(s.clone()).monospace().size(T_SMALL), Some(&s));
                    }
                    None => readout(ui, p, w, dim(p, "None chosen"), None),
                }
                if ui.add_enabled(!busy && !picking, Button::new("Choose…").min_size(vec2(0.0, 32.0))).clicked() {
                    acts.push(Action::ChooseCookie);
                }
                let can_clear = !busy && self.settings.cookie_file.is_some();
                if ui.add_enabled(can_clear, Button::new("Clear").min_size(vec2(0.0, 32.0))).clicked() {
                    self.settings.cookie_file = None;
                }
            });
            ui.label(dim(
                p,
                "Not needed for public profiles. Only sent to pawchive.pw. Only the path is remembered; the file's contents are never shown or saved.",
            ));
            if busy {
                ui.add_space(S2);
                ui.label(RichText::new("ⓘ  Contact and cookie file are locked while a scan or download is running.").size(T_SMALL));
            }
            ui.add_space(S3);
            ui.separator();
            ui.add_space(S2);
            ui.horizontal(|ui| {
                if secondary(ui, "Reset to defaults", 0.0, true).clicked() {
                    self.reset_settings();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if primary(ui, p, "Done", 120.0, true).clicked() {
                        close = true;
                    }
                });
            });
        });
        if modal.should_close() {
            close = true;
        }
        for a in acts {
            self.perform(a, ctx);
        }
        if close {
            self.settings_open = false;
            self.sync_settings(ctx, true);
        }
    }

    pub(crate) fn quit_modal(&mut self, ctx: &egui::Context, p: &Pal) {
        let (saved, total) = (self.model.saved(), self.model.rows.len());
        let (mut quit, mut keep) = (false, false);
        let modal = egui::Modal::new(egui::Id::unique("quit")).backdrop_color(p.backdrop()).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(RichText::new("Quit while downloading?").size(T_TITLE).strong());
            ui.add_space(S2);
            ui.label(format!(
                "{saved} of {total} files are saved. Files in progress keep their partial data, so the next run resumes them instead of starting over."
            ));
            ui.add_space(S4);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // The safe choice is the primary one and sits where the eye lands first.
                keep = primary(ui, p, "Keep downloading", 164.0, true).clicked();
                quit = secondary(ui, "Quit", 92.0, true).clicked();
            });
        });
        if keep || modal.should_close() {
            self.quit_open = false;
        }
        if quit {
            self.quit_open = false;
            self.confirm_quit(ctx);
        }
    }
}
