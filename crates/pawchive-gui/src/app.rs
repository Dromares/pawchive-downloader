use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;

use eframe::egui::{self, Color32, RichText};
use pawchive_core::classify::{Category, CustomExts};
use pawchive_core::plan::{summarize, FileSummary, Selection};
use pawchive_core::session::{Command, SessionHandle, UiEvent};
use pawchive_core::settings::{Settings, MAX_CONCURRENCY, MIN_DELAY_MS};

use crate::dev;
use crate::format::{human_size, size_cell};
use crate::state::{Model, Phase, RowState, Sizing};

pub struct App {
    handle: SessionHandle,
    rx: Receiver<UiEvent>,
    model: Model,
    settings: Settings,
    draft: Settings,
    settings_open: bool,
    url: String,
    output: String,
    custom_ext: String,
    checked: HashSet<Category>,
    include_preview: bool,
    summary_cache: Option<(u64, String, FileSummary)>,
    // folder / cookie-file pickers run on their own thread
    picker_tx: Sender<(Picker, Option<PathBuf>)>,
    picker_rx: Receiver<(Picker, Option<PathBuf>)>,
    note_tx: Sender<String>,
    note_rx: Receiver<String>,
    started: Instant,
    shot_done: bool,
}

#[derive(Clone, Copy, Debug)]
enum Picker {
    Output,
    CookieFile,
}

impl App {
    pub fn new(settings: Settings, handle: SessionHandle, rx: Receiver<UiEvent>) -> Self {
        let (picker_tx, picker_rx) = channel();
        let (note_tx, note_rx) = channel();
        let mut app = App {
            handle,
            rx,
            model: Model::default(),
            output: settings.output_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            custom_ext: settings.custom_extensions.clone(),
            draft: settings.clone(),
            settings,
            settings_open: false,
            url: String::new(),
            checked: Category::ALL.into_iter().collect(),
            include_preview: false, // thumbnails are opt-in
            summary_cache: None,
            picker_tx,
            picker_rx,
            note_tx,
            note_rx,
            started: Instant::now(),
            shot_done: false,
        };
        app.settings_open = dev::open_settings();
        if let Some(url) = dev::autoscan_url() {
            app.url = url.clone();
            app.handle.send(Command::Scan { url });
        }
        app
    }

    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            if matches!(ev, UiEvent::ScanFinished { .. }) {
                if let Some(dir) = dev::autodownload_dir() {
                    self.output = dir;
                    self.start_download();
                }
            }
            self.model.apply(ev);
        }
        while let Ok(note) = self.note_rx.try_recv() {
            self.model.push_log(note);
        }
        while let Ok((which, path)) = self.picker_rx.try_recv() {
            let Some(path) = path else { continue };
            match which {
                Picker::Output => self.output = path.display().to_string(),
                Picker::CookieFile => self.draft.cookie_file = Some(path),
            }
        }
    }

    fn pick(&self, ctx: &egui::Context, which: Picker) {
        let (tx, ctx) = (self.picker_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            // Blocking call into the XDG desktop portal, on its own thread.
            let path = match which {
                Picker::Output => rfd::FileDialog::new().set_title("Choose the output folder").pick_folder(),
                Picker::CookieFile => rfd::FileDialog::new().set_title("Choose a cookies.txt file").pick_file(),
            };
            let _ = tx.send((which, path));
            ctx.request_repaint();
        });
    }

    fn summary(&mut self) -> FileSummary {
        let key = (self.model.files_version, self.custom_ext.clone());
        if let Some((v, c, s)) = &self.summary_cache {
            if (*v, c) == (key.0, &key.1) {
                return s.clone();
            }
        }
        let s = summarize(&self.model.files, &CustomExts::parse(&self.custom_ext));
        self.summary_cache = Some((key.0, key.1, s.clone()));
        s
    }

    fn save_settings(&mut self) {
        self.settings.output_dir = (!self.output.trim().is_empty()).then(|| PathBuf::from(self.output.trim()));
        self.settings.custom_extensions = self.custom_ext.clone();
        if let Err(e) = self.settings.save() {
            self.model.push_log(format!("Could not save settings: {e}"));
        }
    }

    fn start_download(&mut self) {
        self.save_settings();
        let selection = Selection { categories: self.checked.clone(), include_preview: self.include_preview };
        self.handle.send(Command::StartDownload {
            selection,
            custom_extensions: self.custom_ext.clone(),
            output: PathBuf::from(self.output.trim()),
        });
    }

    // ---- screen sections --------------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Pawchive Downloader");
            ui.label(RichText::new(format!("v{}", pawchive_core::VERSION)).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    self.draft = self.settings.clone();
                    self.settings_open = true;
                }
            });
        });
    }

    fn step_scan(&mut self, ui: &mut egui::Ui) {
        ui.strong("1. Creator URL");
        ui.horizontal(|ui| {
            let busy = self.model.busy();
            let edit = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut self.url)
                    .hint_text("https://pawchive.pw/fanbox/user/1386751")
                    .desired_width(ui.available_width() - 90.0),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.add_enabled(!busy, egui::Button::new("Scan")).clicked() || (enter && !busy) {
                self.handle.send(Command::Scan { url: self.url.clone() });
            }
        });
        match self.model.phase {
            Phase::Scanning => {
                let p = self.model.scan_progress;
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Scanning: {} pages, {} posts, {} files so far", p.pages, p.posts, p.files));
                    if ui.button("Cancel").clicked() {
                        self.handle.send(Command::Cancel);
                    }
                });
            }
            Phase::Ready | Phase::Downloading | Phase::Finished => {
                if let Some(c) = &self.model.creator {
                    ui.label(format!(
                        "{}/{}: {} posts, {} files",
                        c.service.as_str(),
                        c.id,
                        self.model.posts,
                        self.model.files.len()
                    ));
                }
            }
            Phase::Idle => {}
        }
        if let Some(e) = &self.model.error {
            ui.colored_label(Color32::from_rgb(220, 90, 90), e);
        }
    }

    fn step_types(&mut self, ui: &mut egui::Ui) {
        ui.strong("2. What to download");
        if self.model.files.is_empty() {
            ui.label(RichText::new("Scan a creator to see their files.").weak());
            return;
        }
        let summary = self.summary();
        let sizing = match self.model.sizing {
            Sizing::Running { done, total } => Some((done, total)),
            _ => None,
        };
        let enabled = !self.model.busy();
        ui.add_enabled_ui(enabled, |ui| {
            egui::Grid::new("types").num_columns(3).striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
                for cat in Category::ALL {
                    let stat = summary.per_category.get(&cat);
                    let count = stat.map_or(0, |s| s.count);
                    if count == 0 {
                        continue; // only show types this creator actually has
                    }
                    let mut on = self.checked.contains(&cat);
                    if ui.checkbox(&mut on, cat.label()).changed() {
                        if on {
                            self.checked.insert(cat);
                        } else {
                            self.checked.remove(&cat);
                        }
                    }
                    ui.label(format!("{count}"));
                    ui.label(stat.map(|s| size_cell(s, sizing)).unwrap_or_default());
                    ui.end_row();
                }
                // Preview-only files are never presented as downloadable originals.
                ui.add_enabled(
                    summary.preview.count > 0,
                    egui::Checkbox::new(&mut self.include_preview, "Preview only (thumbnail)"),
                );
                ui.label(format!("{}", summary.preview.count));
                ui.label("");
                ui.end_row();
            });
            if summary.preview.count > 0 {
                ui.label(
                    RichText::new(
                        "Pawchive does not hold the full-size files for these. Ticking this saves the small \
                         thumbnails next to the originals as name.thumb.ext.",
                    )
                    .small()
                    .weak(),
                );
            }
            ui.horizontal(|ui| {
                ui.label("Custom extensions:");
                ui.add(egui::TextEdit::singleline(&mut self.custom_ext).hint_text("psd, clip").desired_width(160.0));
                ui.label(RichText::new("matched first, e.g. psd,mp4").small().weak());
            });
        });
        match self.model.sizing {
            Sizing::Running { done, total } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "Sizing files in the background ({done}/{total}); downloading works without sizes."
                    ));
                    if ui.button("Skip sizing").clicked() {
                        self.handle.send(Command::SkipSizing);
                    }
                });
            }
            Sizing::Idle if self.model.phase == Phase::Ready => {
                ui.label(RichText::new("Sizing starts in a moment…").weak());
            }
            _ => {}
        }
    }

    fn step_output(&mut self, ui: &mut egui::Ui) {
        ui.strong("3. Output folder");
        ui.horizontal(|ui| {
            ui.add_enabled(
                !self.model.busy(),
                egui::TextEdit::singleline(&mut self.output)
                    .hint_text("/path/to/folder")
                    .desired_width(ui.available_width() - 100.0),
            );
            if ui.add_enabled(!self.model.busy(), egui::Button::new("Browse…")).clicked() {
                self.pick(&ui.ctx().clone(), Picker::Output);
            }
        });
        if !self.output.trim().is_empty() {
            ui.label(
                RichText::new(format!("Files go to {}/<service>_<creator>/<date>_<post>_<title>/", self.output.trim()))
                    .small()
                    .weak(),
            );
        }
    }

    fn step_download(&mut self, ui: &mut egui::Ui) {
        ui.strong("4. Download");
        let summary = self.summary();
        let selected: usize = Category::ALL
            .iter()
            .filter(|c| self.checked.contains(c))
            .map(|c| summary.per_category.get(c).map_or(0, |s| s.count))
            .sum::<usize>()
            + if self.include_preview { summary.preview.count } else { 0 };
        let can_start = self.model.phase != Phase::Scanning
            && self.model.phase != Phase::Downloading
            && !self.model.files.is_empty()
            && selected > 0
            && !self.output.trim().is_empty();
        ui.horizontal(|ui| {
            if ui.add_enabled(can_start, egui::Button::new(format!("Download {selected} files"))).clicked() {
                self.start_download();
            }
            let downloading = self.model.phase == Phase::Downloading;
            if self.model.paused {
                if ui.add_enabled(downloading, egui::Button::new("Resume")).clicked() {
                    self.model.paused = false;
                    self.handle.send(Command::Resume);
                }
            } else if ui.add_enabled(downloading, egui::Button::new("Pause")).clicked() {
                self.model.paused = true;
                self.handle.send(Command::Pause);
            }
            if ui.add_enabled(downloading, egui::Button::new("Cancel")).clicked() {
                self.handle.send(Command::Cancel);
            }
            if !can_start && self.model.phase == Phase::Ready {
                let why = if selected == 0 { "tick something above" } else { "choose an output folder" };
                ui.label(RichText::new(why).weak());
            }
        });
        if let Some(n) = &self.model.notice {
            ui.colored_label(Color32::from_rgb(230, 170, 60), n);
        }
        let total = self.model.rows.len();
        if total == 0 {
            return;
        }
        let finished = self.model.finished_rows();
        let frac = finished as f32 / total as f32;
        let text = format!(
            "{finished}/{total} files, {}{}",
            human_size(self.model.bytes_done()),
            if self.model.paused { " (paused)" } else { "" }
        );
        ui.add(egui::ProgressBar::new(frac).text(text).desired_height(18.0));
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        egui::ScrollArea::vertical().id_salt("rows").max_height(210.0).auto_shrink([false, true]).show_rows(
            ui,
            row_h,
            total,
            |ui, range| {
                for i in range {
                    let r = &self.model.rows[i];
                    ui.horizontal(|ui| {
                        let (text, color) = match &r.state {
                            RowState::Queued => ("queued".to_string(), Color32::GRAY),
                            RowState::Active => (progress_text(r.done, r.total), Color32::LIGHT_BLUE),
                            RowState::Retrying(why) => (format!("retrying ({why})"), Color32::from_rgb(230, 170, 60)),
                            RowState::Done => ("done".into(), Color32::from_rgb(110, 200, 120)),
                            RowState::Skipped => ("skipped (exists)".into(), Color32::GRAY),
                            RowState::Failed { error, .. } => {
                                (format!("failed: {error}"), Color32::from_rgb(220, 90, 90))
                            }
                            RowState::Cancelled => ("cancelled".into(), Color32::GRAY),
                        };
                        ui.allocate_ui_with_layout(
                            egui::vec2(210.0, row_h),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| ui.add(egui::Label::new(RichText::new(text).color(color)).truncate()),
                        );
                        ui.add(egui::Label::new(&r.label).truncate());
                    });
                }
            },
        );
    }

    fn step_log(&mut self, ui: &mut egui::Ui) {
        ui.strong("5. Log and failures");
        let failed: Vec<(usize, String, String)> = self
            .model
            .failed_rows()
            .map(|(i, r)| {
                (
                    i,
                    r.label.clone(),
                    match &r.state {
                        RowState::Failed { error, .. } => error.clone(),
                        _ => String::new(),
                    },
                )
            })
            .collect();
        if !failed.is_empty() {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(220, 90, 90), format!("{} files failed", failed.len()));
                let idle = self.model.phase != Phase::Downloading;
                if ui.add_enabled(idle, egui::Button::new("Retry failed")).clicked() {
                    self.handle.send(Command::RetryFailed);
                }
            });
            egui::ScrollArea::vertical().id_salt("failed").max_height(90.0).auto_shrink([false, true]).show(ui, |ui| {
                for (_, label, err) in &failed {
                    ui.label(RichText::new(format!("{label} — {err}")).small());
                }
            });
        }
        egui::ScrollArea::vertical()
            .id_salt("log")
            .max_height(130.0)
            .auto_shrink([false, true])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.model.log {
                    ui.label(RichText::new(line).small().monospace());
                }
            });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        let mut apply = false;
        egui::Window::new("Settings").min_width(460.0).open(&mut open).collapsible(false).resizable(false).show(
            ctx,
            |ui| {
                egui::Grid::new("settings").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                    ui.label("Concurrent downloads");
                    ui.add(egui::Slider::new(&mut self.draft.concurrency, 1..=MAX_CONCURRENCY));
                    ui.end_row();
                    ui.label("Minimum delay between requests");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.draft.delay_ms).range(MIN_DELAY_MS..=60_000).suffix(" ms"),
                        );
                        ui.label(RichText::new("the site asks for at most 1 request/second").small().weak());
                    });
                    ui.end_row();
                    ui.label("Contact (added to the user agent)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.contact)
                            .hint_text("you@example.org")
                            .desired_width(260.0),
                    );
                    ui.end_row();
                    ui.label("Cookie file (optional)");
                    ui.horizontal(|ui| {
                        let shown = self
                            .draft
                            .cookie_file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "none".into());
                        ui.label(shown);
                        if ui.button("Choose…").clicked() {
                            self.pick(ctx, Picker::CookieFile);
                        }
                        if ui.add_enabled(self.draft.cookie_file.is_some(), egui::Button::new("Clear")).clicked() {
                            self.draft.cookie_file = None;
                        }
                    });
                    ui.end_row();
                });
                ui.label(
                RichText::new(
                    "A Netscape cookies.txt is read into memory only. Its contents are never logged, shown or saved; \
                     only the file path is remembered. Pawchive works without it.",
                )
                .small()
                .weak(),
            );
                ui.label(
                    RichText::new(format!("User agent: {}", self.draft.clone().normalized().user_agent()))
                        .small()
                        .monospace(),
                );
                ui.add_space(6.0);
                if ui.button("Apply").clicked() {
                    apply = true;
                }
            },
        );
        if apply {
            self.settings = std::mem::take(&mut self.draft).normalized();
            self.save_settings();
            self.handle.send(Command::SetSettings(self.settings.clone()));
            open = false;
        }
        self.settings_open = open;
    }

    fn dev_hooks(&mut self, ctx: &egui::Context) {
        if dev::screenshots_enabled() && ctx.input(|i| i.key_pressed(egui::Key::F12)) {
            dev::request_screenshot(ctx, self.note_tx.clone());
        }
        if let Some(secs) = dev::shot_after_secs() {
            if !self.shot_done && self.started.elapsed().as_secs_f64() >= secs {
                self.shot_done = true;
                dev::request_screenshot(ctx, self.note_tx.clone());
            } else if !self.shot_done {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
    }
}

fn progress_text(done: u64, total: Option<u64>) -> String {
    match total {
        Some(t) if t > 0 => format!("{:.0}%  {} / {}", done as f64 * 100.0 / t as f64, human_size(done), human_size(t)),
        _ => human_size(done),
    }
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.dev_hooks(&ctx);
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("page").show(ui, |ui| {
                ui.add_space(4.0);
                self.step_scan(ui);
                ui.separator();
                self.step_types(ui);
                ui.separator();
                self.step_output(ui);
                ui.separator();
                self.step_download(ui);
                ui.separator();
                self.step_log(ui);
            });
        });
        if self.settings_open {
            self.settings_window(&ctx);
        }
    }
}
