//! In-app folder/file browser for Android (android-only module, `mod`-gated in `lib.rs`).
//!
//! Why this exists: desktop uses `rfd` (XDG portal) and neither a portal nor SAF is usable here —
//! SAF hands out `content://` URIs, which the download engine's `std::fs` writes cannot open.
//! The pragmatic, sideload-honest answer is Android's "All files access" permission plus a plain
//! path browser over real filesystem paths: the engine keeps writing ordinary paths, and a real
//! permission button (not a silent dead button) explains and opens the settings page when a
//! directory is off-limits.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, RichText, ScrollArea, Window};

use crate::app::Picker;
use crate::theme::Pal;

pub(crate) enum PickMode {
    Folders,
    Files,
}

pub(crate) struct FolderPicker {
    pub(crate) which: Picker,
    pub(crate) mode: PickMode,
    pub(crate) path: PathBuf,
    entries: Vec<(String, bool)>,
    note: Option<String>,
    /// Set when the user picked a path (or cancelled); `ui()` applies it and closes the modal.
    pub(crate) done: Option<Option<PathBuf>>,
}

/// True when we can actually create a file in `dir` — the only gauge Android respects.
fn writable(dir: &Path) -> bool {
    let probe = dir.join(".pawchive_rw_probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl FolderPicker {
    pub(crate) fn new(which: Picker, mode: PickMode, path: PathBuf) -> Self {
        let mut picker = Self { which, mode, path, entries: Vec::new(), note: None, done: None };
        picker.refresh();
        picker
    }

    fn refresh(&mut self) {
        self.note = None;
        let mut entries = Vec::new();
        match std::fs::read_dir(&self.path) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    if matches!(self.mode, PickMode::Folders) && !is_dir {
                        continue;
                    }
                    entries.push((name, is_dir));
                }
                entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
            }
            Err(why) => self.note = Some(format!("Can't read this folder: {why}.")),
        }
        self.entries = entries;
    }

    pub(crate) fn show(&mut self, ctx: &egui::Context, p: &Pal, screen_rect: egui::Rect) {
        let title = if matches!(self.mode, PickMode::Folders) { "Choose a folder" } else { "Choose a file" };
        let _ = ctx;
        let screen = screen_rect;
        let mut open = self.done.is_none();
        Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width((screen.width() * 0.92).min(560.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("^ ..").clicked() {
                        if let Some(up) = self.path.parent() {
                            self.path = up.to_path_buf();
                            self.refresh();
                        }
                    }
                    ui.add(egui::Label::new(RichText::new(self.path.display().to_string()).weak().size(12.0)).truncate());
                });
                let blocked = !writable(&self.path);
                if blocked || self.note.is_some() {
                    if let Some(note) = &self.note {
                        ui.label(RichText::new(note).color(p.bad).size(12.0));
                    }
                    if blocked {
                        ui.label(
                            RichText::new(
                                "Android is blocking writes here. To unlock it: system Settings -> Apps -> \
                                 Special access -> All files access -> Pawchive Downloader -> Allow, then \
                                 press “Check again”.",
                            )
                            .color(p.bad)
                            .size(12.0),
                        );
                        if ui.button("Check again").clicked() {
                            self.refresh();
                        }
                    }
                }
                let height = (screen.height() * 0.5).clamp(160.0, 520.0);
                ScrollArea::vertical().max_height(height).auto_shrink([false, false]).show(ui, |ui| {
                    for (name, is_dir) in self.entries.clone() {
                        let full = if is_dir { format!("{name}/") } else { name.clone() };
                        // Long names must not stretch the window: egui windows can never shrink
                        // below their content's minimum width, so truncate in the string layer.
                        let label: String = if full.chars().count() > 44 {
                            let head: String = full.chars().take(42).collect();
                            format!("{head}...")
                        } else {
                            full.clone()
                        };
                        let w = ui.available_width();
                        if ui.add_sized(egui::vec2(w, 30.0), egui::Button::new(label)).clicked() {
                            if is_dir {
                                self.path = self.path.join(&name);
                                self.refresh();
                            } else {
                                self.done = Some(Some(self.path.join(&name)));
                            }
                        }
                    }
                    if self.entries.is_empty() && self.note.is_none() {
                        ui.label(RichText::new("Empty.").weak());
                    }
                });
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if matches!(self.mode, PickMode::Folders) {
                        let raw =
                            self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| {
                                self.path.display().to_string()
                            });
                        let name: String = if raw.chars().count() > 24 {
                            let head: String = raw.chars().take(22).collect();
                            format!("{head}...")
                        } else {
                            raw
                        };
                        let choose = ui.button(format!("Choose “{name}”"));
                        if choose.clicked() {
                            if blocked {
                                self.note = Some("That folder is not writable.".to_owned());
                            } else {
                                self.done = Some(Some(self.path.clone()));
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.done = Some(None);
                    }
                });
            });
        if self.done.is_none() && !open {
            self.done = Some(None);
        }
    }
}
