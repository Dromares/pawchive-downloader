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
use pawchive_core::creator::CreatorRef;
use pawchive_core::export;
use pawchive_core::model::FileEntry;
use pawchive_core::names::{creator_dir, post_dir};
use pawchive_core::plan::{category_of, missing_with_thumbnail, plan_all, summarize, FileSummary, Selection};
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
    /// Scan, or Update when the link is a saved profile.
    Scan,
    CancelScan,
    Download,
    /// Fetch only the files the library has not downloaded.
    DownloadNew,
    /// Read the whole post list of the open saved profile again.
    FullRescan,
    /// Open the n-th saved profile of the list.
    OpenSaved(usize),
    /// Ask before removing the n-th saved profile.
    AskRemove(usize),
    ConfirmRemove,
    KeepProfile,
    /// Back to the saved-profiles list.
    BackToList,
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
    /// Check the shown links that were never checked.
    CheckLinks,
    SkipChecking,
    /// Save every found link; the menu item decides the format.
    ExportList(ExportFormat),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Txt,
    Csv,
}

impl ExportFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ExportFormat::Txt => "txt",
            ExportFormat::Csv => "csv",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Files,
    Links,
    Posts,
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

/// Outcome of an export, reported back from the dialog thread.
type ExportDone = Result<PathBuf, String>;

/// A row of the "what will be saved" preview shown before a download starts.
pub struct ReadyRow {
    pub post: String,
    pub name: String,
    pub size: Option<u64>,
    /// The server answered 404 for this file when it was sized: listed, but it will not be saved.
    pub missing: bool,
    /// The library has it as downloaded (saved profiles only), and where.
    pub downloaded: bool,
    pub saved_path: Option<PathBuf>,
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
    export_tx: Sender<ExportDone>,
    export_rx: Receiver<ExportDone>,
    last_export_dir: Option<PathBuf>,
    /// Links to ask the session to check (ticked rows, or the Check links button).
    pub(crate) link_checks: Vec<usize>,
    /// One-line result of the last export, shown in the Links toolbar until the next action.
    pub(crate) link_notice: Option<(Level, String)>,
    /// Row (in the visible list) with the keyboard ring, and the list's last scroll offset and height.
    pub(crate) link_focus: usize,
    pub(crate) link_view: (f32, f32),
    /// The saved profile whose row shows "Remove from your list?".
    pub(crate) remove_confirm: Option<CreatorRef>,
    /// Keep (the safe button) has taken the focus for the confirm row now showing.
    pub(crate) confirm_focused: bool,
    new_cache: Option<(String, Sel)>,
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
        Self::with_context(&cc.egui_ctx, settings, handle, rx)
    }

    /// `new` without the eframe window, so a test can drive the whole UI on a bare egui context.
    fn with_context(ctx: &egui::Context, settings: Settings, handle: SessionHandle, rx: Receiver<UiEvent>) -> Self {
        theme::install_fonts(ctx);
        theme::install_styles(ctx);
        let (picker_tx, picker_rx) = channel();
        let (note_tx, note_rx) = channel();
        let (export_tx, export_rx) = channel();
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
            export_tx,
            export_rx,
            last_export_dir: None,
            link_checks: Vec::new(),
            link_notice: None,
            link_focus: 0,
            link_view: (0.0, 0.0),
            remove_confirm: None,
            confirm_focused: false,
            new_cache: None,
            started: Instant::now(),
            shot_done: false,
            last_title: String::new(),
        };
        if let Some(url) = dev::autoscan_url() {
            app.url = url.clone();
            if dev::full_rescan() {
                // The saved profile is not on screen yet, so this goes straight to the session.
                app.handle.send(Command::FullRescan { url });
            } else {
                app.perform(Action::Scan, ctx);
            }
        }
        if let Some(link) = dev::autoopen() {
            if let Ok(c) = pawchive_core::creator::parse_creator_url(&link) {
                app.url = Self::creator_url(&c);
                app.handle.send(Command::OpenProfile(c));
            }
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

    /// Pawchive files plus the ticked link files (each counted once, section 5 of the links spec).
    pub(crate) fn selection(&mut self) -> Sel {
        let summary = self.summary();
        let sel = derive::selection(&summary, &self.checked, self.include_preview);
        derive::with_links(sel, &crate::links_state::totals(&self.model.links))
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
                let marks = self.model.saved.as_ref().map(|s| &s.marks);
                let mut rows: Vec<ReadyRow> = plan_all(
                    creator,
                    &self.model.files,
                    &sel,
                    &CustomExts::parse(&self.custom_ext),
                    Path::new(""),
                    &Hosts::default(),
                    None,
                )
                .into_iter()
                .map(|x| {
                    let mark = x.file.and_then(|i| marks.and_then(|m| m.get(i)));
                    let (post, name) = x.job.label.rsplit_once('/').unwrap_or(("", x.job.label.as_str()));
                    let downloaded = mark.is_some_and(|m| m.downloaded);
                    let saved_path = mark.and_then(|m| m.saved_path.clone());
                    // A saved file is shown under the name it has on disk.
                    let name = match (&saved_path, downloaded) {
                        (Some(p), true) => p.file_name().map_or(name.to_string(), |n| n.to_string_lossy().into_owned()),
                        _ => name.to_string(),
                    };
                    ReadyRow {
                        post: post.to_string(),
                        name,
                        size: x.job.expected_size,
                        missing: false,
                        downloaded,
                        saved_path,
                    }
                })
                .collect();
                // Files to fetch first, then the ones already saved (a stable sort keeps the site's order).
                rows.sort_by_key(|r| r.downloaded);
                // Gone files stay visible, marked, after the ones that will be saved. A missing image
                // whose thumbnail is ticked is already above, as a `.thumb` job.
                let thumb_saved = |f: &FileEntry| self.include_preview && missing_with_thumbnail(f);
                rows.extend(self.model.files.iter().filter(|f| f.missing && !thumb_saved(f)).map(|f| ReadyRow {
                    post: post_dir(f.post_published.as_deref(), &f.post_id, &f.post_title),
                    name: f.name.clone(),
                    size: None,
                    missing: true,
                    downloaded: false,
                    saved_path: None,
                }));
                rows
            }
            None => Vec::new(),
        };
        let rows = Arc::new(rows);
        self.ready_cache = Some((key, rows.clone()));
        rows
    }

    /// What "Download new only" would fetch among the ticked types: the files the library has not
    /// downloaded (`pending`). Thumbnails and external links are not part of it.
    pub(crate) fn new_only(&mut self) -> Sel {
        let mut cats: Vec<_> = self.checked.iter().copied().collect();
        cats.sort();
        let key = format!("{}|{}|{cats:?}", self.model.files_version, self.custom_ext);
        if let Some((k, s)) = &self.new_cache {
            if *k == key {
                return *s;
            }
        }
        let mut sel = Sel::default();
        if let Some(saved) = &self.model.saved {
            let wanted = derive::wanted(&self.checked);
            let custom = CustomExts::parse(&self.custom_ext);
            for (f, m) in self.model.files.iter().zip(&saved.marks) {
                if m.pending && !f.missing && wanted.contains(&category_of(f, &custom)) {
                    sel.files += 1;
                    match f.size {
                        Some(b) => sel.bytes += b,
                        None => sel.unsized_files += 1,
                    }
                }
            }
            sel.partial = sel.unsized_files > 0;
        }
        self.new_cache = Some((key, sel));
        sel
    }

    /// The URL button says `Update` when the link is a saved profile (or one is open and the field
    /// is not a creator link), else `Scan`.
    pub(crate) fn updating_link(&self) -> bool {
        if !self.model.library_on() {
            return false;
        }
        match crate::urlcheck::check(&self.url) {
            Ok(t) => t.post_id.is_none() && self.model.is_saved(&t.creator),
            Err(_) => self.model.saved.is_some(),
        }
    }

    /// The saved profile the link in the field names, if any.
    pub(crate) fn typed_saved(&self) -> Option<&pawchive_core::library::ProfileRecord> {
        let t = crate::urlcheck::check(&self.url).ok()?;
        if t.post_id.is_some() {
            return None;
        }
        self.model.record_of(&t.creator)
    }

    /// `https://pawchive.pw/{service}/user/{id}` of a creator.
    fn creator_url(c: &CreatorRef) -> String {
        format!("https://pawchive.pw/{}/user/{}", c.service.as_str(), c.id)
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
                let free = derive::download_block(self.has_folder(), sel.files, self.all_gone()).is_none();
                if self.model.saved.as_ref().is_some_and(|s| s.has_history()) {
                    // The primary button is "Download new only"; it is off when there is nothing new.
                    return (free && self.new_only().files > 0).then_some(Action::DownloadNew);
                }
                free.then_some(Action::Download)
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
            let scan_done = match &ev {
                UiEvent::ScanFinished { .. } => true,
                UiEvent::ProfileLoaded(p) => !matches!(p.how, pawchive_core::session::Loaded::Updated(_)),
                _ => false,
            };
            let removed = matches!(ev, UiEvent::ProfileRemoved { .. });
            let loaded = matches!(ev, UiEvent::ProfileLoaded(_));
            self.model.apply(ev, now);
            if removed {
                self.remove_confirm = None;
            }
            if dev::confirm_remove() && self.remove_confirm.is_none() && self.model.phase == Phase::Empty {
                self.remove_confirm = self.model.profiles().first().map(|p| p.creator.clone());
            }
            if scan_done {
                self.tab = if dev::open_links_tab() { Tab::Links } else { Tab::Files };
                self.filter = Filter::All;
                self.link_notice = None;
                self.link_focus = 0;
                if let Some(which) = dev::tick_links() {
                    for i in 0..self.model.links.len() {
                        let host = self.model.links[i].link.host.name().to_lowercase();
                        if which == "1" || host.contains(&which.to_lowercase()) {
                            self.tick_link(i, true);
                        }
                    }
                } else if dev::check_links() {
                    self.link_checks.extend((0..self.model.links.len()).filter(|&i| self.model.links[i].needs_check()));
                }
                if let Some(dir) = dev::autodownload_dir() {
                    self.output = dir;
                    self.perform(if dev::download_new_only() { Action::DownloadNew } else { Action::Download }, ctx);
                }
            }
            // (After the scan_done reset above: a developer's tab choice wins.)
            if loaded {
                if let Some(t) = dev::tab() {
                    self.tab = match t.as_str() {
                        "posts" => Tab::Posts,
                        "links" => Tab::Links,
                        "problems" => Tab::Problems,
                        "log" => Tab::Log,
                        _ => Tab::Files,
                    };
                }
            }
        }
        while let Ok(done) = self.export_rx.try_recv() {
            self.link_notice = Some(match done {
                Ok(path) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.last_export_dir = path.parent().map(Path::to_path_buf);
                    let n = self.model.links.len();
                    (Level::Info, format!("Saved {n} link{} to …/{name}", if n == 1 { "" } else { "s" }))
                }
                Err(e) => (Level::Error, format!("Couldn't save the list: {e}")),
            });
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
                    Ok(target) => {
                        self.url_error = None;
                        let mut url = format!(
                            "https://pawchive.pw/{}/user/{}",
                            target.creator.service.as_str(),
                            target.creator.id
                        );
                        if let Some(post) = &target.post_id {
                            url.push_str(&format!("/post/{post}"));
                        }
                        self.handle.send(Command::Scan { url });
                    }
                    Err(e) => self.url_error = Some(e),
                }
            }
            Action::FullRescan => {
                // Only for the saved profile that is open, and never while something runs.
                if !self.url_editable() || self.model.saved.is_none() {
                    return;
                }
                if let Some(c) = self.model.creator.clone() {
                    self.url_error = None;
                    self.handle.send(Command::FullRescan { url: Self::creator_url(&c) });
                }
            }
            Action::OpenSaved(i) => {
                if self.model.phase != Phase::Empty {
                    return;
                }
                if let Some(c) = self.model.profiles().get(i).map(|p| p.creator.clone()) {
                    self.url = Self::creator_url(&c);
                    self.url_error = None;
                    self.remove_confirm = None;
                    self.handle.send(Command::OpenProfile(c));
                }
            }
            Action::AskRemove(i) => {
                self.remove_confirm = self.model.profiles().get(i).map(|p| p.creator.clone());
                self.confirm_focused = false;
            }
            Action::KeepProfile => self.remove_confirm = None,
            Action::ConfirmRemove => {
                if let Some(c) = self.remove_confirm.take() {
                    self.handle.send(Command::RemoveProfile(c));
                }
            }
            Action::BackToList => {
                if self.model.phase.busy() || !self.model.library_on() {
                    return;
                }
                self.model.close_profile();
                self.model.removed_note = None;
                self.remove_confirm = None;
                self.url_error = None;
            }
            Action::CancelScan | Action::Cancel => {
                self.model.note_cancel();
                self.handle.send(Command::Cancel);
            }
            Action::DownloadNew => {
                if self.model.phase != Phase::Ready {
                    return;
                }
                self.sync_settings(ctx, true);
                self.handle.send(Command::DownloadNew {
                    selection: Selection {
                        categories: derive::wanted(&self.checked),
                        include_preview: self.include_preview,
                    },
                    custom_extensions: self.custom_ext.clone(),
                    output: PathBuf::from(self.output.trim()),
                    links: self.model.links.iter().filter_map(crate::links_state::LinkRow::pick).collect(),
                });
                self.link_notice = None;
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
                    links: self.model.links.iter().filter_map(crate::links_state::LinkRow::pick).collect(),
                });
                self.link_notice = None;
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
            Action::CheckLinks => {
                let ids: Vec<usize> = crate::links_state::shown(&self.model.links, self.model.link_filter)
                    .into_iter()
                    .filter(|&i| self.model.links[i].needs_check())
                    .collect();
                self.link_checks.extend(ids);
            }
            Action::SkipChecking => self.handle.send(Command::SkipChecking),
            Action::ExportList(fmt) => self.export_list(fmt, ctx),
        }
    }

    /// Ask the session to check the links the UI queued this frame.
    fn flush_link_checks(&mut self) {
        if self.link_checks.is_empty() {
            return;
        }
        let mut ids = std::mem::take(&mut self.link_checks);
        ids.sort_unstable();
        ids.dedup();
        if matches!(self.model.phase, Phase::Ready) {
            self.handle.send(Command::CheckLinks { links: ids });
        }
    }

    /// Save every found link (ticks and the host filter do not matter) with a native save dialog.
    /// The dialog and the write run on their own thread; the result comes back as a toolbar line.
    fn export_list(&mut self, fmt: ExportFormat, ctx: &egui::Context) {
        let (Some(creator), false) = (self.model.creator.clone(), self.model.links.is_empty()) else { return };
        if matches!(self.model.phase, Phase::Scanning | Phase::Empty | Phase::ScanError) {
            return;
        }
        let post_id = self.model.scan_target.as_ref().and_then(|t| t.post_id.clone());
        let name = export::default_file_name(creator.service.as_str(), &creator.id, post_id.as_deref(), fmt.ext());
        let bytes: Vec<u8> = {
            let m = &self.model;
            let links: Vec<_> = m.links.iter().map(|r| r.link.clone()).collect();
            match fmt {
                ExportFormat::Txt => export::to_txt(&links).into_bytes(),
                ExportFormat::Csv => export::to_csv(&links, creator.service.as_str(), &creator.id, &|l| {
                    crate::links_state::export_info(&m.links[l.id], m.link_outcome(l.id))
                }),
            }
        };
        if let Some(d) = dev::export_dir() {
            // Developer shortcut: no dialog, the file goes straight into this folder.
            let path = PathBuf::from(d).join(&name);
            let _ = self.export_tx.send(std::fs::write(&path, bytes).map(|()| path).map_err(|e| e.to_string()));
            return;
        }
        let dir = self
            .last_export_dir
            .clone()
            .or_else(|| self.has_folder().then(|| PathBuf::from(self.output.trim())).filter(|p| p.is_dir()));
        let (tx, ctx) = (self.export_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut dlg = rfd::FileDialog::new().set_title("Save the list of links").set_file_name(name);
            if let Some(d) = dir {
                dlg = dlg.set_directory(d);
            }
            // Cancel: nothing happens, no message.
            if let Some(path) = dlg.save_file() {
                // The chosen menu item decides the format, whatever extension the dialog returned.
                let path = path.with_extension(fmt.ext());
                let _ = tx.send(std::fs::write(&path, bytes).map(|()| path).map_err(|e| e.to_string()));
                ctx.request_repaint();
            }
        });
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
        let (focus_url, browse, primary, pause, settings, quit, update) =
            (hit(Key::L), hit(Key::O), hit(Key::Enter), hit(Key::P), hit(Key::Comma), hit(Key::Q), hit(Key::U));
        if self.settings_open || self.quit_open {
            return;
        }
        if focus_url {
            self.focus_url = true;
        }
        if browse {
            self.perform(Action::Browse, ctx);
        }
        // Ctrl+U runs the URL button when it says Update.
        if update && self.updating_link() && !self.url.trim().is_empty() {
            self.perform(Action::Scan, ctx);
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
        if let Some(secs) = dev::cancel_after_secs() {
            if self.model.phase == Phase::Scanning && self.started.elapsed().as_secs_f64() >= secs {
                self.perform(Action::CancelScan, ctx);
            }
            ctx.request_repaint_after(Duration::from_millis(200));
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
        self.draw(ui);
    }
}

impl App {
    fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let pal = Pal::new(ctx.theme().into());
        self.dev_hooks(&ctx);
        self.shortcuts(&ctx);
        let mut acts: Vec<Action> = Vec::new();
        let panel =
            |l, r, t, b| Frame::new().fill(pal.bg).inner_margin(Margin { left: l, right: r, top: t, bottom: b });
        let (m, s) = (S4 as i8, 10);

        egui::Panel::top("title").frame(panel(m, m, 12, s)).show(ui, |ui| self.titlebar(ui, &pal, &mut acts));
        // Tab order is widget creation order (UI-SPEC §7: the bottom bar is last), but the bar's panel
        // must be declared before the central area to claim its strip. So the panel only reserves the
        // strip; the bar's widgets are created after everything else, into that rect.
        let bar = egui::Panel::bottom("action")
            .frame(panel(m, m, s, s))
            .show(ui, |ui| {
                ui.allocate_exact_size(egui::vec2(ui.available_width(), theme::BTN_H), egui::Sense::hover()).0
            })
            .inner;
        egui::Panel::left("setup").exact_size(LEFT_W).resizable(false).frame(panel(m, m, 14, 0)).show(ui, |ui| {
            self.left(ui, &pal, &mut acts);
        });
        egui::CentralPanel::default().frame(panel(m, m, 14, 12)).show(ui, |ui| self.centre(ui, &pal, &mut acts));
        self.action_bar(&mut ui.new_child(egui::UiBuilder::new().max_rect(bar)), &pal, &mut acts);

        if self.settings_open {
            self.settings_modal(&ctx, &pal);
        }
        if self.quit_open {
            self.quit_modal(&ctx, &pal);
        }
        for a in acts {
            self.perform(a, &ctx);
        }
        self.flush_link_checks();
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

    /// Walks Tab through the real UI on a bare egui context, headless, and names each stop by its
    /// accessibility label (UI-SPEC §7: Browse… before the tabs, the bottom bar last). `with_file`
    /// adds an output folder and one file to the scan, so Download is enabled and tabbable.
    fn tab_walk(ready: bool, with_file: bool) -> Vec<String> {
        use pawchive_core::creator::CreatorRef;
        use pawchive_core::model::Service;
        use std::collections::HashMap;

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let (handle, rx) = pawchive_core::session::spawn(
            Settings::default(),
            Hosts::default(),
            pawchive_core::session::Tuning::default(),
            Arc::new(|| {}),
        );
        let mut app = App::with_context(&ctx, Settings::default(), handle, rx);
        if ready {
            let creator = CreatorRef { service: Service::Fanbox, id: "1".into() };
            app.model.apply(
                UiEvent::ScanStarted(pawchive_core::creator::ScanTarget { creator: creator.clone(), post_id: None }),
                Instant::now(),
            );
            let file = || pawchive_core::model::FileEntry {
                post_id: "1".into(),
                post_title: "t".into(),
                post_published: None,
                name: "a.jpg".into(),
                path: "/a.jpg".into(),
                preview_only: false,
                node: None,
                size: Some(10),
                content_type: Some("image/jpeg".into()),
                missing: false,
            };
            let files = if with_file { vec![file()] } else { vec![] };
            app.model
                .apply(UiEvent::ScanFinished { creator, posts: 1, files, links: vec![], post: None }, Instant::now());
            if with_file {
                app.output = "/tmp/pawchive-tab-walk".into();
            }
        }
        let mut nodes: HashMap<egui::accesskit::NodeId, String> = HashMap::new();
        let mut frame = |app: &mut App, tab: bool| -> Option<String> {
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 760.0))),
                ..Default::default()
            };
            if tab {
                input.events.push(egui::Event::Key {
                    key: Key::Tab,
                    physical_key: Some(Key::Tab),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                });
            }
            let mut out = ctx.run_ui(input, |ui| app.draw(ui));
            let update = out.platform_output.accesskit_update.take();
            out.drop_without_applying_deltas();
            let update = update?;
            for (id, node) in update.nodes {
                let name =
                    node.label().or_else(|| node.value()).map_or_else(|| format!("{:?}", node.role()), str::to_string);
                nodes.insert(id, name);
            }
            nodes.get(&update.focus).cloned()
        };
        for _ in 0..3 {
            frame(&mut app, false);
        }
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..40 {
            frame(&mut app, true);
            let Some(name) = frame(&mut app, false) else { continue };
            if seen.first() == Some(&name) {
                break; // wrapped around
            }
            seen.push(name);
        }
        seen
    }

    /// DAN-1565, UI-SPEC §7: a first run needs the folder picker before Download enables, so Browse…
    /// comes right after the Extensions field and before the tabs, not after the whole file area.
    #[test]
    fn tab_reaches_browse_straight_after_extensions_and_before_the_tabs() {
        for ready in [false, true] {
            let order = tab_walk(ready, false);
            let at = |name: &str| order.iter().position(|n| n == name);
            let browse = at("Browse…").unwrap_or_else(|| panic!("Browse… is not tabbable: {order:?}"));
            // The text fields are the unlabelled stops: the link field, then Extensions once a scan
            // has made step 2 live.
            let fields: Vec<_> = order.iter().enumerate().filter(|(_, n)| n.is_empty()).map(|(i, _)| i).collect();
            assert_eq!(fields.len(), if ready { 2 } else { 1 }, "text fields: {order:?}");
            assert_eq!(browse, fields[fields.len() - 1] + 1, "Browse… must follow the last field: {order:?}");
            if ready {
                assert!(browse < at("Files 0").unwrap(), "Browse… must come before the tabs: {order:?}");
            }
        }
    }

    /// DAN-1573, UI-SPEC §7: the bottom bar is the last stop. Egui's Tab order is widget creation
    /// order and the bar's panel is declared before the central area, so it has to reserve its slot.
    #[test]
    fn tab_ends_on_the_bottom_bar_after_the_tabs_and_the_table() {
        let order = tab_walk(true, true);
        let at =
            |name: &str| order.iter().position(|n| n == name).unwrap_or_else(|| panic!("{name} missing: {order:?}"));
        let download = at("Download 1");
        assert_eq!(download, order.len() - 1, "bottom bar must come last: {order:?}");
        assert!(at("Browse…") < at("Files 1") && at("Files 1") < download, "{order:?}");
    }

    // ---- External links tab ---------------------------------------------------------------------------

    fn link_fixture(
        id: usize,
        host: pawchive_core::links::Host,
        kind: pawchive_core::links::LinkKind,
    ) -> pawchive_core::links::Link {
        pawchive_core::links::Link {
            id,
            url: format!("https://x.test/{id}"),
            host,
            kind,
            name_hint: None,
            posts: vec![pawchive_core::links::PostRef {
                id: "1".into(),
                title: "T".into(),
                published: Some("2026-01-01T00:00:00".into()),
            }],
        }
    }

    /// An app on the External links tab with a Drive folder (3 files), a Mediafire file and a Mega link.
    fn links_app(ctx: &egui::Context) -> App {
        use pawchive_core::creator::{CreatorRef, ScanTarget};
        use pawchive_core::external::{Child, Resolved, ResolvedKind};
        use pawchive_core::links::{Host, LinkKind};
        use pawchive_core::model::Service;
        let (handle, rx) = pawchive_core::session::spawn(
            Settings::default(),
            Hosts::default(),
            pawchive_core::session::Tuning::default(),
            Arc::new(|| {}),
        );
        let mut app = App::with_context(ctx, Settings::default(), handle, rx);
        let creator = CreatorRef { service: Service::Patreon, id: "1".into() };
        let now = Instant::now();
        app.model.apply(UiEvent::ScanStarted(ScanTarget { creator: creator.clone(), post_id: None }), now);
        let links = vec![
            link_fixture(0, Host::Drive, LinkKind::DriveFolder { id: "F".into() }),
            link_fixture(1, Host::Mediafire, LinkKind::MediafireFile { key: "k".into(), page_url: "p".into() }),
            link_fixture(2, Host::Mega, LinkKind::Unsupported { why: "Mega isn't supported".into() }),
        ];
        app.model.apply(UiEvent::ScanFinished { creator, posts: 1, files: vec![], links, post: None }, now);
        let kids = (0..3)
            .map(|i| Child { id: format!("c{i}"), dirs: vec![], name: format!("{i}.jpg"), size: Some(10), mime: None })
            .collect();
        let folder = Resolved {
            name: "Folder".into(),
            size: Some(30),
            content_type: None,
            kind: ResolvedKind::Folder { children: kids, partial: false, capped: false },
        };
        app.model.apply(UiEvent::LinkResolved { link: 0, resolved: folder }, now);
        let file = Resolved { name: "a.rar".into(), size: Some(5), content_type: None, kind: ResolvedKind::File };
        app.model.apply(UiEvent::LinkResolved { link: 1, resolved: file }, now);
        app.tab = Tab::Links;
        app
    }

    fn key_event(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers }
    }

    fn frame_with(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 760.0))),
            events,
            ..Default::default()
        };
        let out = ctx.run_ui(input, |ui| app.draw(ui));
        out.drop_without_applying_deltas();
    }

    /// Section 2.3: the list is one Tab stop and the keys work inside it: Space ticks (never a dead or
    /// unsupported row), Right/Left open and close a folder (Left on a child goes to its parent), Home/End
    /// jump, Ctrl+A selects the shown rows.
    #[test]
    fn the_links_list_is_driven_from_the_keyboard() {
        use crate::links_state::Tri;
        let ctx = egui::Context::default();
        let mut app = links_app(&ctx);
        for _ in 0..3 {
            frame_with(&ctx, &mut app, vec![]);
        }
        ctx.memory_mut(|m| m.request_focus(egui::Id::unique("external_links_list")));
        frame_with(&ctx, &mut app, vec![]);
        let press = |ctx: &egui::Context, app: &mut App, k: Key, m: Modifiers| {
            frame_with(ctx, app, vec![key_event(k, m)]);
            frame_with(ctx, app, vec![]);
        };
        press(&ctx, &mut app, Key::Space, Modifiers::NONE);
        assert_eq!(app.model.links[0].tri(), Tri::All, "Space ticks the folder: all its files");
        press(&ctx, &mut app, Key::ArrowRight, Modifiers::NONE);
        assert!(app.model.links[0].open, "Right opens the folder");
        press(&ctx, &mut app, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(app.link_focus, 1);
        press(&ctx, &mut app, Key::Space, Modifiers::NONE);
        assert_eq!(app.model.links[0].tri(), Tri::Some, "one child off: the parent shows the dash");
        assert_eq!(app.model.links[0].selected(), 2);
        press(&ctx, &mut app, Key::ArrowLeft, Modifiers::NONE);
        assert_eq!(app.link_focus, 0, "Left on a child goes to its parent");
        press(&ctx, &mut app, Key::ArrowLeft, Modifiers::NONE);
        assert!(!app.model.links[0].open, "Left on an open folder closes it");
        press(&ctx, &mut app, Key::End, Modifiers::NONE);
        assert_eq!(app.link_focus, 2);
        press(&ctx, &mut app, Key::Space, Modifiers::NONE);
        assert_eq!(app.model.links[2].selected(), 0, "an unsupported link is never ticked");
        press(&ctx, &mut app, Key::Home, Modifiers::NONE);
        assert_eq!(app.link_focus, 0);
        press(&ctx, &mut app, Key::Enter, Modifiers::NONE);
        assert!(app.model.links[0].open, "Enter opens a folder");
        press(&ctx, &mut app, Key::A, Modifiers::COMMAND);
        assert_eq!(app.model.links[1].selected(), 1, "Ctrl+A selects the shown rows");
        assert_eq!(app.model.links[0].tri(), Tri::All);
        assert_eq!(app.model.links[2].selected(), 0);
    }

    /// Section 1.4: Check links, Export list, the four chips, the header checkbox, then the list as ONE
    /// stop, and the bottom bar last.
    #[test]
    fn tab_order_in_the_links_tab_ends_with_one_list_stop_then_the_bottom_bar() {
        use std::collections::HashMap;
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = links_app(&ctx);
        app.output = "/tmp/pawchive-tab-walk".into();
        // One unchecked, ticked link: Check links and Download are both live.
        app.model.links[1].check = crate::links_state::Check::NotChecked;
        app.model.links[1].set_on(true);
        let mut nodes: HashMap<egui::accesskit::NodeId, String> = HashMap::new();
        let mut frame = |app: &mut App, tab: bool| -> Option<String> {
            let events = if tab { vec![key_event(Key::Tab, Modifiers::NONE)] } else { vec![] };
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 760.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| app.draw(ui));
            let update = out.platform_output.accesskit_update.take();
            out.drop_without_applying_deltas();
            let update = update?;
            for (id, node) in update.nodes {
                let name =
                    node.label().or_else(|| node.value()).map_or_else(|| format!("{:?}", node.role()), str::to_string);
                nodes.insert(id, name);
            }
            nodes.get(&update.focus).cloned()
        };
        for _ in 0..3 {
            frame(&mut app, false);
        }
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..40 {
            frame(&mut app, true);
            let Some(name) = frame(&mut app, false) else { continue };
            if seen.first() == Some(&name) {
                break;
            }
            seen.push(name);
        }
        let at = |name: &str| seen.iter().position(|n| n == name).unwrap_or_else(|| panic!("{name} missing: {seen:?}"));
        let (check, export) = (at("Check links"), at("Export list    "));
        let (first_chip, last_chip) = (at("All 3"), at("Not downloadable 1"));
        let header = at("Select all shown links");
        let download = at("Download 1");
        assert!(check < export && export < first_chip && first_chip < last_chip, "{seen:?}");
        assert!(last_chip < header, "{seen:?}");
        assert!(
            header + 2 == download,
            "exactly one stop (the list) between the header checkbox and the bottom bar: {seen:?}"
        );
        assert_eq!(download, seen.len() - 1, "bottom bar last: {seen:?}");
    }

    // ---- DAN-1616: cool-down exit, silent Download, "1 files" ------------------------------------------

    /// Every accessibility label/value drawn over three frames (the first carries the whole tree).
    fn labels(ctx: &egui::Context, app: &mut App) -> Vec<String> {
        let mut out = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 760.0))),
                ..Default::default()
            };
            let mut o = ctx.run_ui(input, |ui| app.draw(ui));
            if let Some(update) = o.platform_output.accesskit_update.take() {
                out.extend(
                    update.nodes.iter().filter_map(|(_, n)| n.label().or_else(|| n.value()).map(str::to_string)),
                );
            }
            o.drop_without_applying_deltas();
        }
        out
    }

    /// F1: a Drive cool-down takes the toolbar's left slot, and Skip used to vanish with it, so a check
    /// stuck behind a 429 could only be left by quitting.
    #[test]
    fn skip_stays_reachable_while_a_check_waits_out_a_cooldown() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = links_app(&ctx);
        let now = Instant::now();
        app.model.apply(UiEvent::CheckProgress { done: 0, total: 1 }, now);
        app.model.apply(UiEvent::HostCooldown { host: pawchive_core::external::ExtHost::Drive, secs: 60 }, now);
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l.starts_with("Google Drive asked us to slow down")), "{seen:?}");
        assert!(seen.iter().any(|l| l == "Skip"), "Skip must stay on screen during a cool-down: {seen:?}");
    }

    /// F2: Download pressed while picked links are still being checked shows why nothing is downloading
    /// yet, and offers Cancel instead of a dead Download button.
    #[test]
    fn download_waiting_for_checks_says_so_in_the_bottom_bar() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = links_app(&ctx);
        app.output = "/tmp/pawchive-awaiting".into();
        let now = Instant::now();
        app.model.apply(UiEvent::CheckProgress { done: 1, total: 3 }, now);
        app.model.apply(UiEvent::DownloadWaiting, now);
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l.starts_with("Checking links before downloading · 1 of 3")), "{seen:?}");
        assert!(seen.iter().any(|l| l == "Cancel"), "{seen:?}");
        assert!(!seen.iter().any(|l| l.starts_with("Download ")), "no live Download button while waiting: {seen:?}");
        // The wait ending without a download puts the Download button back.
        app.model.apply(UiEvent::DownloadFinished(Default::default()), now);
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l.starts_with("Download ")), "{seen:?}");
    }

    /// F3: one file is "1 file", in the bottom bar as everywhere else.
    #[test]
    fn the_bottom_bar_says_1_file_not_1_files() {
        use pawchive_core::creator::{CreatorRef, ScanTarget};
        use pawchive_core::model::Service;
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = links_app(&ctx);
        app.output = "/tmp/pawchive-one-file".into();
        let creator = CreatorRef { service: Service::Patreon, id: "1".into() };
        let now = Instant::now();
        app.model.apply(UiEvent::ScanStarted(ScanTarget { creator: creator.clone(), post_id: None }), now);
        let file = pawchive_core::model::FileEntry {
            post_id: "1".into(),
            post_title: "t".into(),
            post_published: None,
            name: "a.jpg".into(),
            path: "/a.jpg".into(),
            preview_only: false,
            node: None,
            size: Some(1_200_000),
            content_type: Some("image/jpeg".into()),
            missing: false,
        };
        app.model.apply(UiEvent::ScanFinished { creator, posts: 1, files: vec![file], links: vec![], post: None }, now);
        app.tab = Tab::Files;
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l.starts_with("1 file · ")), "{seen:?}");
        assert!(!seen.iter().any(|l| l.contains("1 files")), "{seen:?}");
    }

    // ---- DAN-1661: saved profiles --------------------------------------------------------------------

    use pawchive_core::creator::CreatorRef;
    use pawchive_core::library::{ChangeSummary, PostLabel, PostRow, ProfileRecord};
    use pawchive_core::model::Service;
    use pawchive_core::session::{
        FileMark, Loaded, ProfileLoaded, UiEvent as Ev, UpdateKind, UpdateReport, UpdateStep, UpdateSummary,
    };

    fn creator() -> CreatorRef {
        CreatorRef { service: Service::Fanbox, id: "1549213".into() }
    }

    fn record(gone: usize) -> ProfileRecord {
        ProfileRecord {
            creator: creator(),
            name: Some("DAGASI".into()),
            site_updated: Some("2026-10-17T13:02:44".into()),
            last_scanned_at: Some(unix_now_for_tests() - 3600),
            last_full_read_at: Some(unix_now_for_tests() - 3600),
            posts: 1_427,
            gone,
            last_change: Some(ChangeSummary {
                at: unix_now_for_tests() - 3600,
                new_posts: 12,
                updated_posts: 3,
                new_files: 41,
                gone_posts: 1,
            }),
        }
    }

    fn unix_now_for_tests() -> i64 {
        crate::library_state::unix_now()
    }

    fn entry(post: &str, name: &str, size: Option<u64>, missing: bool) -> FileEntry {
        FileEntry {
            post_id: post.into(),
            post_title: format!("Post {post}"),
            post_published: Some("2026-10-01T00:00:00".into()),
            name: name.into(),
            path: format!("/aa/{name}"),
            preview_only: false,
            node: None,
            size,
            content_type: None,
            missing,
        }
    }

    /// A saved profile on screen: 5 files (2 downloaded, 2 to fetch, 1 not on Pawchive) and 4 posts
    /// carrying every label.
    fn profile_loaded(how: Loaded, gone: usize) -> ProfileLoaded {
        let row = |id: &str, label, files| PostRow {
            post_id: id.into(),
            title: format!("Post {id}"),
            published: Some("2026-10-01T00:00:00".into()),
            label,
            files,
        };
        let mark = |downloaded, pending| FileMark { downloaded, pending, saved_path: None };
        ProfileLoaded {
            how,
            record: record(gone),
            posts: vec![
                row("1", Some(PostLabel::New), 2),
                row("2", Some(PostLabel::Updated), 1),
                row("3", Some(PostLabel::Gone), 1),
                row("4", None, 1),
            ],
            files: vec![
                entry("1", "a.jpg", Some(1_000_000), false),
                entry("1", "b.jpg", Some(2_000_000), false),
                entry("2", "c.jpg", Some(3_000_000), false),
                entry("4", "d.jpg", Some(4_000_000), false),
                entry("3", "e.jpg", None, true),
            ],
            marks: vec![mark(false, true), mark(false, true), mark(true, false), mark(true, false), mark(false, false)],
            links: vec![],
            probing: 0,
        }
    }

    fn saved_app(ctx: &egui::Context, how: Loaded) -> App {
        let (handle, rx) = pawchive_core::session::spawn(
            Settings::default(),
            Hosts::default(),
            pawchive_core::session::Tuning::default(),
            Arc::new(|| {}),
        );
        let mut app = App::with_context(ctx, Settings::default(), handle, rx);
        let now = Instant::now();
        app.model.apply(Ev::Profiles(Ok(vec![record(0)])), now);
        app.model.apply(Ev::ProfileLoaded(Box::new(profile_loaded(how, 1))), now);
        app.output = "/tmp/pawchive-saved".into();
        app
    }

    fn report(summary: UpdateSummary, unchanged: bool) -> Loaded {
        Loaded::Updated(UpdateReport { kind: UpdateKind::Update, unchanged, summary, posts_seen: 1_427, requests: 1 })
    }

    fn has(seen: &[String], text: &str) -> bool {
        seen.iter().any(|l| l.contains(text))
    }

    #[test]
    fn the_saved_list_names_each_profile_and_asks_before_removing() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let (handle, rx) = pawchive_core::session::spawn(
            Settings::default(),
            Hosts::default(),
            pawchive_core::session::Tuning::default(),
            Arc::new(|| {}),
        );
        let mut app = App::with_context(&ctx, Settings::default(), handle, rx);
        // An empty library: the guide and the first-run card, no list.
        app.model.apply(Ev::Profiles(Ok(vec![])), Instant::now());
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "No saved profiles yet"), "{seen:?}");
        assert!(!has(&seen, "Saved profiles"), "{seen:?}");
        // One saved profile: the list, with the stated copy.
        app.model.apply(Ev::Profiles(Ok(vec![record(2)])), Instant::now());
        let seen = labels(&ctx, &mut app);
        for want in [
            "Saved profiles",
            "DAGASI",
            "fanbox · 1549213",
            "Open",
            "Remove",
            "Today ",
            "Removing a profile from this list",
        ] {
            assert!(has(&seen, want), "{want} missing: {seen:?}");
        }
        assert!(!has(&seen, "No saved profiles yet"));
        // Remove asks first, names what is lost, and mentions the Gone posts when there are some.
        app.perform(Action::AskRemove(0), &ctx);
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Remove \u{201c}DAGASI\u{201d} from your list?"), "{seen:?}");
        assert!(
            has(
                &seen,
                "Your downloaded files stay in your folder. The saved post list and download history are deleted"
            ),
            "{seen:?}"
        );
        assert!(has(&seen, "2 posts marked Gone from site will be forgotten."), "{seen:?}");
        assert!(has(&seen, "Keep"), "{seen:?}");
        // Keep closes it without a request; nothing was removed.
        app.perform(Action::KeepProfile, &ctx);
        assert!(app.remove_confirm.is_none() && app.model.profiles().len() == 1);
        // The footer gives way to the result line after a removal.
        app.model.apply(Ev::ProfileRemoved { creator: creator() }, Instant::now());
        app.model.apply(Ev::Profiles(Ok(vec![record(0)])), Instant::now());
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Removed \u{201c}DAGASI\u{201d}. Downloaded files were kept."), "{seen:?}");
    }

    #[test]
    fn the_url_button_says_update_for_a_saved_link_and_scan_otherwise() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let (handle, rx) = pawchive_core::session::spawn(
            Settings::default(),
            Hosts::default(),
            pawchive_core::session::Tuning::default(),
            Arc::new(|| {}),
        );
        let mut app = App::with_context(&ctx, Settings::default(), handle, rx);
        app.model.apply(Ev::Profiles(Ok(vec![record(0)])), Instant::now());
        app.url = "https://pawchive.pw/fanbox/user/1549213".into();
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l == "Update"), "{seen:?}");
        assert!(has(&seen, "Already in your library · last scanned Today"), "{seen:?}");
        app.url = "https://pawchive.pw/fanbox/user/999".into();
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l == "Scan") && !seen.iter().any(|l| l == "Update"), "{seen:?}");
        // A post link of a saved creator is a one-post lookup: still Scan, and never saved.
        app.url = "https://pawchive.pw/fanbox/user/1549213/post/5".into();
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l == "Scan"), "{seen:?}");
        // Without a library nothing changes from v0.2.0.
        let (handle, rx) = pawchive_core::session::spawn(
            Settings::default(),
            Hosts::default(),
            pawchive_core::session::Tuning::default(),
            Arc::new(|| {}),
        );
        let mut plain = App::with_context(&ctx, Settings::default(), handle, rx);
        plain.url = "https://pawchive.pw/fanbox/user/1549213".into();
        assert!(!plain.updating_link());
    }

    #[test]
    fn an_opened_profile_says_what_it_is_and_offers_full_rescan_and_the_way_back() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = saved_app(&ctx, Loaded::Opened);
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Saved profile — 1,427 posts, 5 files"), "{seen:?}");
        assert!(has(&seen, "2 new files selected · "), "{seen:?}");
        assert!(has(&seen, "Last scanned Today"), "{seen:?}");
        assert!(has(&seen, "Last update found 12 new posts, 3 updated, 41 new files, 1 gone."), "{seen:?}");
        assert!(has(&seen, "DAGASI · fanbox 1549213 · 1,427 posts"), "{seen:?}");
        for want in ["Full rescan", "Saved profiles", "Posts 4"] {
            assert!(seen.iter().any(|l| l == want || l.starts_with(want)), "{want}: {seen:?}");
        }
        assert!(seen.iter().any(|l| l == "Update"));
    }

    /// DAN-1676: with no measured size among the files to fetch the bar read `≥ 0 B`, while the card
    /// beside it (and every other place) said `size not measured`.
    #[test]
    fn the_new_only_bar_says_size_not_measured_instead_of_zero_bytes() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = saved_app(&ctx, Loaded::Opened);
        let mut unsized_files = profile_loaded(Loaded::Opened, 1);
        unsized_files.files.iter_mut().for_each(|f| f.size = None);
        app.model.apply(Ev::ProfileLoaded(Box::new(unsized_files)), Instant::now());
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "2 new files · size not measured"), "{seen:?}");
        assert!(has(&seen, "2 new files selected · size not measured"), "{seen:?}");
        assert!(!has(&seen, "≥ 0 B"), "{seen:?}");
    }

    #[test]
    fn the_bottom_bar_offers_new_only_once_there_is_history() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = saved_app(&ctx, Loaded::Opened);
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l == "Download new only (2)"), "{seen:?}");
        assert!(seen.iter().any(|l| l == "Download all (4)"), "downloadable files, not the gone one: {seen:?}");
        assert_eq!(app.primary_action(), Some(Action::DownloadNew), "Ctrl+Enter presses the primary");
        // Nothing new: the primary is off, "Download all" stays on, and the reason is said.
        let loaded = ProfileLoaded {
            marks: profile_loaded(Loaded::Opened, 0)
                .marks
                .into_iter()
                .map(|m| FileMark { pending: false, ..m })
                .collect(),
            ..profile_loaded(Loaded::Opened, 0)
        };
        app.model.apply(Ev::ProfileLoaded(Box::new(loaded)), Instant::now());
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l == "Download new only (0)"), "{seen:?}");
        assert!(has(&seen, "Nothing new to download."), "{seen:?}");
        assert_eq!(app.primary_action(), None);
        // No history: exactly today's single button.
        let fresh = ProfileLoaded {
            marks: profile_loaded(Loaded::First, 0)
                .marks
                .into_iter()
                .map(|m| FileMark { downloaded: false, ..m })
                .collect(),
            ..profile_loaded(Loaded::First, 0)
        };
        app.model.apply(Ev::ProfileLoaded(Box::new(fresh)), Instant::now());
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l == "Download 4"), "{seen:?}");
        assert!(!has(&seen, "Download new only") && !has(&seen, "Download all"), "{seen:?}");
        assert_eq!(app.primary_action(), Some(Action::Download));
    }

    #[test]
    fn an_update_shows_its_steps_keeps_the_old_data_and_ends_on_the_summary() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = saved_app(&ctx, Loaded::Opened);
        let now = Instant::now();
        app.model.apply(Ev::UpdateStarted { kind: UpdateKind::Update, creator: creator() }, now);
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Checking the profile…"), "{seen:?}");
        assert!(has(&seen, "One request to Pawchive: has anything changed since Today"), "{seen:?}");
        // Controls: Cancel replaces the URL button, Full rescan and the way back are off, the old data stays.
        assert!(seen.iter().any(|l| l == "Cancel") && seen.iter().any(|l| l == "Cancel scan"), "{seen:?}");
        assert!(has(&seen, "Updating. Download unlocks when it finishes."), "{seen:?}");
        assert!(has(&seen, "Posts 4") && has(&seen, "Files 5"), "the previous data stays: {seen:?}");
        assert!(!has(&seen, "Download new only") && !has(&seen, "Download all"), "{seen:?}");
        assert!(!has(&seen, "The file list appears when the scan finishes."), "{seen:?}");
        assert!(!seen.iter().any(|l| l == "…"), "saved counts stay, not …: {seen:?}");
        assert_eq!(crate::derive::window_title(&app.model, now), "Checking… — Pawchive Downloader");
        // Reading: the counter, the estimate and the promise.
        app.model.apply(Ev::UpdateStep(UpdateStep::Reading), now);
        app.model.apply(
            Ev::UpdateStep(UpdateStep::Page(pawchive_core::api::ScanProgress { pages: 14, posts: 700, files: 3000 })),
            now,
        );
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Reading the post list…"), "{seen:?}");
        assert!(has(&seen, "page 14 of about 29 · 700 posts read"), "{seen:?}");
        assert!(
            has(&seen, "About 29 s at one request per second. Your saved copy isn't changed until this finishes."),
            "{seen:?}"
        );
        assert_eq!(crate::derive::window_title(&app.model, now), "Updating… page 14 of about 29 — Pawchive Downloader");
        // Missing posts.
        app.model.apply(Ev::UpdateStep(UpdateStep::CheckingMissing { done: 1, total: 3 }), now);
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Checking posts missing from the list…") && has(&seen, "1 of 3 posts checked"), "{seen:?}");
        assert!(
            has(&seen, "Each is checked on its own before it's marked Gone from site. Nothing is changed yet."),
            "{seen:?}"
        );
        // Done: the summary is the headline.
        let summary = UpdateSummary { new_posts: 12, updated_posts: 3, new_files: 41, gone_posts: 1 };
        app.model.apply(Ev::ProfileLoaded(Box::new(profile_loaded(report(summary, false), 1))), now);
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l == "12 new posts, 3 updated, 41 new files, 1 gone"), "{seen:?}");
        assert!(
            has(&seen, "Updated Today") && has(&seen, "Open the Posts tab to see which posts changed."),
            "{seen:?}"
        );
        assert_eq!(app.tab, Tab::Files, "the app never switches tabs by itself");
        // Nothing found: the zero case, both wordings.
        let zero = UpdateSummary::default();
        app.model.apply(Ev::ProfileLoaded(Box::new(profile_loaded(report(zero, true), 1))), now);
        let seen = labels(&ctx, &mut app);
        assert!(seen.iter().any(|l| l == "Up to date — 0 new posts, 0 updated"), "{seen:?}");
        // Two files are still waiting (the fixture), so "0 new files" would contradict the bottom bar's
        // "Download new only (2)" (DAN-1676).
        assert!(
            has(&seen, "No new files in this update. 2 files not downloaded yet, 2 already downloaded."),
            "{seen:?}"
        );
        assert!(!has(&seen, "0 new files."), "{seen:?}");
        assert!(seen.iter().any(|l| l == "Download new only (2)"), "{seen:?}");
        assert!(
            has(&seen, "Pawchive reports no changes since 2026-10-17 13:02. Full rescan checks every post anyway."),
            "{seen:?}"
        );
        app.model.apply(Ev::ProfileLoaded(Box::new(profile_loaded(report(zero, false), 1))), now);
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "All 1,427 posts were read and none changed."), "{seen:?}");
        // Everything downloaded: the spec's wording stands.
        let mut done = profile_loaded(report(zero, true), 1);
        done.marks.iter_mut().for_each(|m| m.pending = false);
        app.model.apply(Ev::ProfileLoaded(Box::new(done)), now);
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "0 new files. 2 files are already downloaded."), "{seen:?}");
        assert!(seen.iter().any(|l| l == "Download new only (0)"), "{seen:?}");
    }

    #[test]
    fn a_failed_or_cancelled_update_says_nothing_was_lost() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = saved_app(&ctx, Loaded::Opened);
        let now = Instant::now();
        app.model.apply(Ev::UpdateStarted { kind: UpdateKind::Full, creator: creator() }, now);
        app.model.apply(
            Ev::UpdateStep(UpdateStep::Page(pawchive_core::api::ScanProgress { pages: 13, posts: 650, files: 1 })),
            now,
        );
        app.model.apply(Ev::UpdateFailed { cause: "request failed: operation timed out".into(), saving: false }, now);
        assert_eq!(app.model.phase, Phase::Ready);
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Update stopped at page 14"), "{seen:?}");
        assert!(has(&seen, "Couldn't reach pawchive.pw (connection timed out)."), "{seen:?}");
        assert!(has(&seen, "Nothing was changed. Your saved copy is intact. Update again starts over."), "{seen:?}");
        assert!(has(&seen, "Posts 4"), "the old data is still on screen: {seen:?}");
        for word in ["lost", "discarded", "aren't kept", "retry continues"] {
            assert!(!seen.iter().any(|l| l.contains(word)), "forbidden word {word}: {seen:?}");
        }
        app.model.apply(Ev::UpdateStarted { kind: UpdateKind::Update, creator: creator() }, now);
        app.model.apply(Ev::UpdateCancelled, now);
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Update cancelled") && has(&seen, "Nothing was changed."), "{seen:?}");
        assert!(has(&seen, "Your saved copy from Today") && has(&seen, "is intact: 1,427 posts, 5 files."), "{seen:?}");
        // A write that failed at the end has its own headline.
        app.model.apply(Ev::UpdateStarted { kind: UpdateKind::Update, creator: creator() }, now);
        app.model.apply(Ev::UpdateFailed { cause: "library database: disk full".into(), saving: true }, now);
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Couldn't save the update") && has(&seen, "library database: disk full"), "{seen:?}");
        // The next scan clears the notice.
        app.model.apply(Ev::UpdateStarted { kind: UpdateKind::Update, creator: creator() }, now);
        assert!(app.model.notice.is_none());
    }

    #[test]
    fn the_posts_tab_shows_the_labels_the_anchor_and_filters_without_reordering() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = saved_app(&ctx, Loaded::Opened);
        app.tab = Tab::Posts;
        let seen = labels(&ctx, &mut app);
        for want in ["Label: New", "Label: Updated", "Label: × Gone from site", "All 4", "New 1", "Updated 1", "Gone 1"]
        {
            assert!(has(&seen, want), "{want} missing: {seen:?}");
        }
        assert!(has(&seen, "Labels from the update on Today"), "{seen:?}");
        assert!(has(&seen, "· 1 gone"), "{seen:?}");
        app.model.post_filter = crate::library_state::PostFilter::Gone;
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Label: × Gone from site") && !has(&seen, "Label: New"), "{seen:?}");
        // An empty filter says so.
        let mut none = profile_loaded(Loaded::Opened, 0);
        none.posts.retain(|p| p.label != Some(PostLabel::Gone));
        app.model.apply(Ev::ProfileLoaded(Box::new(none)), Instant::now());
        app.model.post_filter = crate::library_state::PostFilter::Gone;
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "No posts match this filter."), "{seen:?}");
        // After the baseline scan there is nothing to label yet.
        let mut base = profile_loaded(Loaded::First, 0);
        base.record.last_change = None;
        for p in &mut base.posts {
            p.label = None;
        }
        app.model.apply(Ev::ProfileLoaded(Box::new(base)), Instant::now());
        app.tab = Tab::Posts;
        let seen = labels(&ctx, &mut app);
        assert!(
            has(&seen, "No labels yet. New, Updated and Gone from site appear after your next update."),
            "{seen:?}"
        );
    }

    #[test]
    fn a_library_that_could_not_be_opened_is_explained_and_the_app_still_scans() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let (handle, rx) = pawchive_core::session::spawn(
            Settings::default(),
            Hosts::default(),
            pawchive_core::session::Tuning::default(),
            Arc::new(|| {}),
        );
        let mut app = App::with_context(&ctx, Settings::default(), handle, rx);
        app.model
            .apply(Ev::Profiles(Err("Couldn't open library.sqlite3: the database is locked.".into())), Instant::now());
        app.url = "https://pawchive.pw/fanbox/user/1549213".into();
        let seen = labels(&ctx, &mut app);
        assert!(has(&seen, "Your library couldn't be opened"), "{seen:?}");
        assert!(has(&seen, "Couldn't open library.sqlite3: the database is locked."), "{seen:?}");
        assert!(
            has(&seen, "You can still scan and download, but profiles won't be saved or listed until this is fixed."),
            "{seen:?}"
        );
        assert!(seen.iter().any(|l| l == "Scan"), "Scan, not Update: {seen:?}");
        assert!(!has(&seen, "No saved profiles yet"), "{seen:?}");
    }

    /// ui-spec-library §3: URL, the URL button, Full rescan, `Saved profiles`, then the type rows.
    #[test]
    fn tab_order_in_step_one_is_url_button_full_rescan_back_link() {
        use std::collections::HashMap;
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = saved_app(&ctx, Loaded::Opened);
        app.url = "https://pawchive.pw/fanbox/user/1549213".into(); // Open fills the field; the button needs a link
        let mut nodes: HashMap<egui::accesskit::NodeId, String> = HashMap::new();
        let mut frame = |app: &mut App, tab: bool| -> Option<String> {
            let events = if tab { vec![key_event(Key::Tab, Modifiers::NONE)] } else { vec![] };
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1120.0, 760.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| app.draw(ui));
            let update = out.platform_output.accesskit_update.take();
            out.drop_without_applying_deltas();
            let update = update?;
            for (id, node) in update.nodes {
                let name =
                    node.label().or_else(|| node.value()).map_or_else(|| format!("{:?}", node.role()), str::to_string);
                nodes.insert(id, name);
            }
            nodes.get(&update.focus).cloned()
        };
        for _ in 0..3 {
            frame(&mut app, false);
        }
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..12 {
            frame(&mut app, true);
            let Some(name) = frame(&mut app, false) else { continue };
            if seen.first() == Some(&name) {
                break;
            }
            seen.push(name);
        }
        let at = |n: &str| seen.iter().position(|x| x == n).unwrap_or_else(|| panic!("{n} missing: {seen:?}"));
        assert!(at("Update") < at("Full rescan") && at("Full rescan") < at("Saved profiles"), "{seen:?}");
        assert!(at("Saved profiles") < at("Images"), "{seen:?}");
    }
}
