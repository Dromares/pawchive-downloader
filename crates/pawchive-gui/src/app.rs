//! The application object: owns the model, the user's choices and the session handle, turns
//! [`Action`]s into session commands, and lays out the window. Drawing lives in `view` and `modals`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Frame, Key, KeyboardShortcut, Margin, Modifiers};
use pawchive_core::api::Hosts;
use pawchive_core::classify::{Category, CustomExts};
use pawchive_core::model::FileEntry;
use pawchive_core::names::{creator_dir, post_dir};
use pawchive_core::plan::{missing_with_thumbnail, plan_downloads, summarize, FileSummary, Selection};
use pawchive_core::session::{Command, SessionHandle, UiEvent};
use pawchive_core::settings::Settings;

use crate::derive::{self, Sel};
use crate::dev;
use crate::state::{Level, Model, Phase, RowState, Sizing};
use crate::theme::{self, Pal, LEFT_W, S4};

/// Everything a click or shortcut can ask for. Drawing code only collects these; `App::perform`
/// is the one place that turns them into model changes and session commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Scan,
    CancelScan,
    Download,
    Pause,
    Resume,
    Cancel,
    RetryFailed,
    RetryOne(usize),
    OpenFolder,
    CopyPath,
    Browse,
    ChooseCookie,
    SkipSizing,
    CopyLog,
    OpenSettings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Files,
    Problems,
    Log,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Active,
    Queued,
    Done,
    Failed,
}

#[derive(Clone, Copy, Debug)]
enum Picker {
    Output,
    CookieFile,
}

/// A row of the "what will be saved" preview shown before a download starts.
pub struct ReadyRow {
    pub post: String,
    pub name: String,
    pub size: Option<u64>,
    /// The server answered 404 for this file when it was sized: listed, but it will not be saved.
    pub missing: bool,
}

pub struct App {
    pub(crate) handle: SessionHandle,
    rx: Receiver<UiEvent>,
    pub(crate) model: Model,
    /// Concurrency, delay, contact and cookie path are edited live in the settings dialog.
    pub(crate) settings: Settings,
    saved: Settings,
    sent: Settings,
    pub(crate) url: String,
    pub(crate) url_error: Option<String>,
    pub(crate) output: String,
    pub(crate) custom_ext: String,
    pub(crate) checked: HashSet<Category>,
    pub(crate) include_preview: bool,
    pub(crate) tab: Tab,
    pub(crate) filter: Filter,
    pub(crate) settings_open: bool,
    pub(crate) quit_open: bool,
    quit_now: bool,
    focus_url: bool,
    summary_cache: Option<(u64, String, Arc<FileSummary>)>,
    ready_cache: Option<(String, Arc<Vec<ReadyRow>>)>,
    picker_tx: Sender<(Picker, Option<PathBuf>)>,
    picker_rx: Receiver<(Picker, Option<PathBuf>)>,
    pub(crate) picker_busy: bool,
    note_tx: Sender<String>,
    note_rx: Receiver<String>,
    started: Instant,
    shot_done: bool,
    last_title: String,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        settings: Settings,
        handle: SessionHandle,
        rx: Receiver<UiEvent>,
    ) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::install_styles(&cc.egui_ctx);
        let (picker_tx, picker_rx) = channel();
        let (note_tx, note_rx) = channel();
        let mut app = App {
            handle,
            rx,
            model: Model::default(),
            output: settings.output_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            custom_ext: settings.custom_extensions.clone(),
            saved: settings.clone(),
            sent: settings.clone().normalized(),
            settings,
            url: String::new(),
            url_error: None,
            checked: derive::default_checked(),
            include_preview: false, // thumbnails are opt-in
            tab: Tab::Files,
            filter: Filter::All,
            settings_open: dev::open_settings(),
            quit_open: false,
            quit_now: false,
            focus_url: false,
            summary_cache: None,
            ready_cache: None,
            picker_tx,
            picker_rx,
            picker_busy: false,
            note_tx,
            note_rx,
            started: Instant::now(),
            shot_done: false,
            last_title: String::new(),
        };
        if let Some(url) = dev::autoscan_url() {
            app.url = url;
            app.perform(Action::Scan, &cc.egui_ctx);
        }
        app
    }

    // ---- derived data -------------------------------------------------------------------------

    pub(crate) fn summary(&mut self) -> Arc<FileSummary> {
        if let Some((v, c, s)) = &self.summary_cache {
            if *v == self.model.files_version && *c == self.custom_ext {
                return s.clone();
            }
        }
        let s = Arc::new(summarize(&self.model.files, &CustomExts::parse(&self.custom_ext)));
        self.summary_cache = Some((self.model.files_version, self.custom_ext.clone(), s.clone()));
        s
    }

    /// Sizes may still arrive or were given up on (Skip): totals are lower bounds, drawn `≥`.
    /// After a normal finish they are final, even if a few probes failed.
    pub(crate) fn sizes_are_lower_bounds(&self) -> bool {
        matches!(self.model.sizing, Sizing::Running { .. } | Sizing::Finished { skipped: true })
    }

    pub(crate) fn selection(&mut self) -> Sel {
        let summary = self.summary();
        derive::selection(&summary, &self.checked, self.include_preview)
    }

    /// Every file the scan found is gone from the server (see [`derive::all_gone`]).
    pub(crate) fn all_gone(&mut self) -> bool {
        derive::all_gone(&self.summary())
    }

    /// The files a download would save right now, named the way they will be on disk.
    pub(crate) fn ready_rows(&mut self) -> Arc<Vec<ReadyRow>> {
        let mut cats: Vec<_> = self.checked.iter().copied().collect();
        cats.sort();
        let key = format!("{}|{}|{cats:?}|{}", self.model.files_version, self.custom_ext, self.include_preview);
        if let Some((k, rows)) = &self.ready_cache {
            if *k == key {
                return rows.clone();
            }
        }
        let rows = match &self.model.creator {
            Some(creator) => {
                let sel =
                    Selection { categories: derive::wanted(&self.checked), include_preview: self.include_preview };
                let mut rows: Vec<ReadyRow> = plan_downloads(
                    creator,
                    &self.model.files,
                    &sel,
                    &CustomExts::parse(&self.custom_ext),
                    Path::new(""),
                    &Hosts::default(),
                )
                .into_iter()
                .map(|j| {
                    let (post, name) = j.label.rsplit_once('/').unwrap_or(("", j.label.as_str()));
                    ReadyRow { post: post.to_string(), name: name.to_string(), size: j.expected_size, missing: false }
                })
                .collect();
                // Gone files stay visible, marked, after the ones that will be saved. A missing image
                // whose thumbnail is ticked is already above, as a `.thumb` job.
                let thumb_saved = |f: &FileEntry| self.include_preview && missing_with_thumbnail(f);
                rows.extend(self.model.files.iter().filter(|f| f.missing && !thumb_saved(f)).map(|f| ReadyRow {
                    post: post_dir(f.post_published.as_deref(), &f.post_id, &f.post_title),
                    name: f.name.clone(),
                    size: None,
                    missing: true,
                }));
                rows
            }
            None => Vec::new(),
        };
        let rows = Arc::new(rows);
        self.ready_cache = Some((key, rows.clone()));
        rows
    }

    pub(crate) fn has_folder(&self) -> bool {
        !self.output.trim().is_empty()
    }

    /// `{output}/{service}_{creator}`: where this profile's files go.
    pub(crate) fn run_folder(&self) -> Option<PathBuf> {
        let c = self.model.creator.as_ref()?;
        self.has_folder().then(|| PathBuf::from(self.output.trim()).join(creator_dir(c.service.as_str(), &c.id)))
    }

    pub(crate) fn url_editable(&self) -> bool {
        matches!(self.model.phase, Phase::Empty | Phase::ScanError | Phase::Ready | Phase::Cancelled | Phase::Finished)
    }

    pub(crate) fn sizing_running(&self) -> bool {
        matches!(self.model.sizing, Sizing::Running { .. })
    }

    /// The bottom bar's primary button, if it has one that Ctrl+Enter may press.
    fn primary_action(&mut self) -> Option<Action> {
        match self.model.phase {
            Phase::Ready => {
                let sel = self.selection();
                derive::download_block(self.has_folder(), sel.files, self.all_gone())
                    .is_none()
                    .then_some(Action::Download)
            }
            Phase::Downloading => Some(Action::Pause),
            Phase::Paused => Some(Action::Resume),
            Phase::ScanError => Some(Action::Scan),
            Phase::Cancelled => (self.model.remaining() > 0).then_some(Action::Download),
            Phase::Finished => Some(if self.model.retryable() > 0 { Action::RetryFailed } else { Action::OpenFolder }),
            Phase::Empty | Phase::Scanning => None,
        }
    }

    // ---- events from the session and the pickers ------------------------------------------------

    fn drain(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        while let Ok(ev) = self.rx.try_recv() {
            let scan_done = matches!(ev, UiEvent::ScanFinished { .. });
            self.model.apply(ev, now);
            if scan_done {
                self.tab = Tab::Files;
                self.filter = Filter::All;
                if let Some(dir) = dev::autodownload_dir() {
                    self.output = dir;
                    self.perform(Action::Download, ctx);
                }
            }
        }
        while let Ok(note) = self.note_rx.try_recv() {
            self.model.push_log(Level::Info, note);
        }
        while let Ok((which, path)) = self.picker_rx.try_recv() {
            self.picker_busy = false;
            let Some(path) = path else { continue };
            match which {
                Picker::Output => self.output = path.display().to_string(),
                Picker::CookieFile => self.settings.cookie_file = Some(path),
            }
        }
    }

    fn pick(&mut self, ctx: &egui::Context, which: Picker) {
        if self.picker_busy {
            return;
        }
        self.picker_busy = true;
        let (tx, ctx) = (self.picker_tx.clone(), ctx.clone());
        // The portal call blocks, so it runs on its own thread and the window keeps repainting.
        std::thread::spawn(move || {
            let path = match which {
                Picker::Output => rfd::FileDialog::new().set_title("Choose the output folder").pick_folder(),
                Picker::CookieFile => rfd::FileDialog::new().set_title("Choose a cookies.txt file").pick_file(),
            };
            let _ = tx.send((which, path));
            ctx.request_repaint();
        });
    }

    // ---- settings persistence --------------------------------------------------------------------

    /// Write the settings file and tell the session when something it uses changed. Skipped while
    /// the user is mid-edit (typing, dragging) so a slider drag or a keystroke is not a write.
    pub(crate) fn sync_settings(&mut self, ctx: &egui::Context, force: bool) {
        if !force && (ctx.egui_wants_keyboard_input() || ctx.egui_is_using_pointer()) {
            return;
        }
        let mut snap = self.settings.clone();
        snap.output_dir = self.has_folder().then(|| PathBuf::from(self.output.trim()));
        snap.custom_extensions = self.custom_ext.clone();
        if snap != self.saved {
            if let Err(e) = snap.save() {
                self.model.push_log(Level::Warn, format!("Could not save settings: {e}"));
            }
            self.saved = snap;
        }
        let norm = self.settings.clone().normalized();
        let changed = (norm.concurrency, norm.delay_ms, &norm.contact, &norm.cookie_file)
            != (self.sent.concurrency, self.sent.delay_ms, &self.sent.contact, &self.sent.cookie_file);
        if changed {
            self.handle.send(Command::SetSettings(norm.clone()));
            self.sent = norm;
        }
    }

    pub(crate) fn reset_settings(&mut self) {
        let d = Settings::default();
        self.settings.concurrency = d.concurrency;
        self.settings.delay_ms = d.delay_ms;
        if !self.model.phase.busy() {
            self.settings.contact = d.contact;
            self.settings.cookie_file = None;
        }
    }

    // ---- actions ----------------------------------------------------------------------------------

    pub(crate) fn perform(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Scan => {
                if !self.url_editable() {
                    return;
                }
                match crate::urlcheck::check(&self.url) {
                    Ok(creator) => {
                        self.url_error = None;
                        let url = format!("https://pawchive.pw/{}/user/{}", creator.service.as_str(), creator.id);
                        self.handle.send(Command::Scan { url });
                    }
                    Err(e) => self.url_error = Some(e),
                }
            }
            Action::CancelScan | Action::Cancel => {
                self.model.note_cancel();
                self.handle.send(Command::Cancel);
            }
            Action::Download => {
                if !matches!(self.model.phase, Phase::Ready | Phase::Cancelled) {
                    return;
                }
                self.sync_settings(ctx, true);
                self.handle.send(Command::StartDownload {
                    selection: Selection {
                        categories: derive::wanted(&self.checked),
                        include_preview: self.include_preview,
                    },
                    custom_extensions: self.custom_ext.clone(),
                    output: PathBuf::from(self.output.trim()),
                });
            }
            Action::Pause => {
                self.model.note_pause();
                self.handle.send(Command::Pause);
            }
            Action::Resume => {
                self.model.note_resume();
                self.handle.send(Command::Resume);
            }
            Action::RetryFailed => {
                let ids: Vec<usize> = self
                    .model
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| matches!(r.state, RowState::Failed { permanent: false, .. }))
                    .map(|(i, _)| i)
                    .collect();
                if !ids.is_empty() && matches!(self.model.phase, Phase::Finished | Phase::Cancelled) {
                    self.model.begin_retry(&ids);
                    self.handle.send(Command::RetryFailed);
                }
            }
            Action::RetryOne(i) => {
                let retryable = self
                    .model
                    .rows
                    .get(i)
                    .is_some_and(|r| matches!(r.state, RowState::Failed { permanent: false, .. }));
                if retryable && matches!(self.model.phase, Phase::Finished | Phase::Cancelled) {
                    self.model.begin_retry(&[i]);
                    self.handle.send(Command::RetryJobs(vec![i as u64]));
                }
            }
            Action::OpenFolder => self.open_folder(ctx),
            Action::CopyPath => {
                if let Some(p) =
                    self.run_folder().or_else(|| self.has_folder().then(|| PathBuf::from(self.output.trim())))
                {
                    ctx.copy_text(p.display().to_string());
                }
            }
            Action::Browse => {
                if !self.model.phase.busy() {
                    self.pick(ctx, Picker::Output);
                }
            }
            Action::ChooseCookie => {
                if !self.model.phase.busy() {
                    self.pick(ctx, Picker::CookieFile);
                }
            }
            Action::SkipSizing => self.handle.send(Command::SkipSizing),
            Action::CopyLog => {
                let text: String =
                    self.model.log.iter().map(|l| format!("{} {}\n", crate::format::hms_utc(l.unix), l.text)).collect();
                ctx.copy_text(text);
            }
            Action::OpenSettings => self.settings_open = true,
        }
    }

    /// `xdg-open` on the profile folder (or the output folder before anything is saved). If it
    /// cannot start, the path goes to the clipboard instead.
    fn open_folder(&mut self, ctx: &egui::Context) {
        let target = match self.run_folder() {
            Some(p) if p.exists() => p,
            _ => PathBuf::from(self.output.trim()),
        };
        match std::process::Command::new("xdg-open").arg(&target).spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => {
                ctx.copy_text(target.display().to_string());
                self.model
                    .push_log(Level::Warn, format!("Could not open the folder ({e}); its path was copied instead"));
            }
        }
    }

    // ---- keyboard ---------------------------------------------------------------------------------

    /// `ui-spec` section 7. Consumed before any widget draws, so they work from a text field too.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let hit = |key: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, key)));
        let (focus_url, browse, primary, pause, settings, quit) =
            (hit(Key::L), hit(Key::O), hit(Key::Enter), hit(Key::P), hit(Key::Comma), hit(Key::Q));
        if self.settings_open || self.quit_open {
            return;
        }
        if focus_url {
            self.focus_url = true;
        }
        if browse {
            self.perform(Action::Browse, ctx);
        }
        if primary {
            if let Some(a) = self.primary_action() {
                self.perform(a, ctx);
            }
        }
        if pause {
            match self.model.phase {
                Phase::Downloading => self.perform(Action::Pause, ctx),
                Phase::Paused => self.perform(Action::Resume, ctx),
                _ => {}
            }
        }
        if settings {
            self.settings_open = true;
        }
        if quit {
            self.request_quit(ctx);
        }
    }

    fn request_quit(&mut self, ctx: &egui::Context) {
        if self.model.phase.running() {
            self.quit_open = true;
        } else {
            self.quit_now = true;
            self.sync_settings(ctx, true);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    pub(crate) fn confirm_quit(&mut self, ctx: &egui::Context) {
        self.quit_now = true;
        self.sync_settings(ctx, true);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn dev_hooks(&mut self, ctx: &egui::Context) {
        if dev::screenshots_enabled() && ctx.input(|i| i.key_pressed(Key::F12)) {
            dev::request_screenshot(ctx, self.note_tx.clone());
        }
        if let Some(secs) = dev::shot_after_secs() {
            if !self.shot_done && self.started.elapsed().as_secs_f64() >= secs {
                self.shot_done = true;
                dev::request_screenshot(ctx, self.note_tx.clone());
            } else if !self.shot_done {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        self.drain(ctx);
        self.model.tick(now);
        if self.model.take_finished() {
            // Once, when a run ends with failures; never otherwise, so it cannot steal focus mid-work.
            if self.model.phase == Phase::Finished && self.model.failed() > 0 {
                self.tab = Tab::Problems;
            }
            if !ctx.input(|i| i.viewport().focused.unwrap_or(true)) {
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Informational,
                ));
            }
        }
        let title = derive::window_title(&self.model, now);
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
        // Spinner, elapsed clock and countdown must advance with no input; idle costs nothing.
        if self.model.phase.busy() || self.sizing_running() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.model.phase.running() && !self.quit_now {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_open = true;
        }
        self.sync_settings(ctx, false);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let pal = Pal::new(ctx.theme().into());
        self.dev_hooks(&ctx);
        self.shortcuts(&ctx);
        let mut acts: Vec<Action> = Vec::new();
        let panel =
            |l, r, t, b| Frame::new().fill(pal.bg).inner_margin(Margin { left: l, right: r, top: t, bottom: b });
        let (m, s) = (S4 as i8, 10);

        egui::Panel::top("title").frame(panel(m, m, 12, s)).show(ui, |ui| self.titlebar(ui, &pal, &mut acts));
        egui::Panel::bottom("action").frame(panel(m, m, s, s)).show(ui, |ui| self.action_bar(ui, &pal, &mut acts));
        egui::Panel::left("setup").exact_size(LEFT_W).resizable(false).frame(panel(m, m, 14, 0)).show(ui, |ui| {
            self.left(ui, &pal, &mut acts);
        });
        egui::CentralPanel::default().frame(panel(m, m, 14, 12)).show(ui, |ui| self.centre(ui, &pal, &mut acts));

        if self.settings_open {
            self.settings_modal(&ctx, &pal);
        }
        if self.quit_open {
            self.quit_modal(&ctx, &pal);
        }
        for a in acts {
            self.perform(a, &ctx);
        }
        if std::mem::take(&mut self.focus_url) {
            crate::view::focus_url(&ctx, self.url.chars().count());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_selection_leaves_other_unticked() {
        let c = derive::default_checked();
        assert!(c.contains(&Category::Image) && c.contains(&Category::Video));
        assert!(!c.contains(&Category::Other));
    }
}
