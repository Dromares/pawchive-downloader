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
        Self::with_context(&cc.egui_ctx, settings, handle, rx)
    }

    /// `new` without the eframe window, so a test can drive the whole UI on a bare egui context.
    fn with_context(ctx: &egui::Context, settings: Settings, handle: SessionHandle, rx: Receiver<UiEvent>) -> Self {
        theme::install_fonts(ctx);
        theme::install_styles(ctx);
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
            app.perform(Action::Scan, ctx);
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
            app.model.apply(UiEvent::ScanStarted(creator.clone()), Instant::now());
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
            app.model.apply(UiEvent::ScanFinished { creator, posts: 1, files }, Instant::now());
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
}
