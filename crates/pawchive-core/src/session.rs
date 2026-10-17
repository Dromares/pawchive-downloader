//! The command/event layer between a front end and the network code. A front end sends
//! [`Command`]s and drains [`UiEvent`]s; everything slow runs on a tokio runtime on its own
//! thread, so the UI thread never blocks and core stays free of GUI dependencies.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::api::{Client, Hosts, ScanProgress};
use crate::classify::CustomExts;
use crate::control::Control;
use crate::cookies::CookieJar;
use crate::creator::{parse_scan_url, CreatorRef, ScanTarget};
use crate::engine::{self, Engine, EngineConfig, Job};
use crate::error::Error;
use crate::external::{self, DeadReason, ExtClient, ExtHost, Notice, ResolveError, Resolved};
use crate::gate::Gate;
use crate::http::{Http, RetryPolicy};
use crate::library::{DownloadTarget, Library, LoadedProfile, PostRow, ProfileRecord, ScanMode, ScanOutcome, ScanStep};
use crate::links::{Link, PostRef};
use crate::model::FileEntry;
use crate::names::{sanitize_component, NameAllocator, MAX_COMPONENT_BYTES};
use crate::plan::{category_of, plan_all, JobOrigin, LinkPick, LinkPlan, Selection};
use crate::probe::probe;
use crate::settings::Settings;

#[derive(Clone, Debug)]
pub enum Command {
    /// Scan a creator (saved in the library when there is one: the first scan of a profile, an
    /// update of a saved one) or one post (never saved).
    Scan {
        url: String,
    },
    /// Read the whole post list of a saved creator again, even if the profile says nothing changed.
    FullRescan {
        url: String,
    },
    /// Show a saved profile from the library. No request is made.
    OpenProfile(CreatorRef),
    /// Forget a saved profile (its post list and download history). Downloaded files stay.
    RemoveProfile(CreatorRef),
    /// Download only the files the library has not downloaded (`StartDownload` fetches every
    /// selected file, skipping those already in the folder).
    DownloadNew {
        selection: Selection,
        custom_extensions: String,
        output: PathBuf,
        links: Vec<LinkPick>,
    },
    SetSettings(Settings),
    /// Stop the background size probing (downloads work without sizes).
    SkipSizing,
    StartDownload {
        selection: Selection,
        custom_extensions: String,
        output: PathBuf,
        /// The external links (or files of a folder) to download as well.
        links: Vec<LinkPick>,
    },
    /// Check these links (name, size, liveness, a folder's files) in the background, one request at
    /// a time per host. Ignored while a download exists.
    CheckLinks {
        links: Vec<usize>,
    },
    /// Stop checking after the current request; the rest stay unchecked.
    SkipChecking,
    Pause,
    Resume,
    /// Cancels the running download, or the running scan. `.part` files are kept for resume.
    Cancel,
    /// Re-run the jobs of the last download that failed *and can still work*: a permanent failure
    /// (HTTP 404, the file is gone) is not retried.
    RetryFailed,
    /// Re-run just these jobs (ids from [`JobInfo`]); ids that did not fail retryably are ignored.
    RetryJobs(Vec<u64>),
}

/// What the UI needs to draw a download row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobInfo {
    pub id: u64,
    pub label: String,
    pub expected_size: Option<u64>,
    pub origin: JobOrigin,
    /// The file name was changed to avoid a clash on disk (`4 (2).png`).
    pub renamed: bool,
}

/// Which scan of a saved profile is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateKind {
    /// Check the profile first; read the list only if it changed (or the last full read is old).
    Update,
    /// Read the whole list now.
    Full,
}

/// What an update found. The same four numbers the summary line shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpdateSummary {
    pub new_posts: usize,
    pub updated_posts: usize,
    pub new_files: usize,
    pub gone_posts: usize,
}

/// A finished update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateReport {
    pub kind: UpdateKind,
    /// The profile was unchanged and its last full read recent: one request, nothing else read.
    pub unchanged: bool,
    pub summary: UpdateSummary,
    pub posts_seen: usize,
    pub requests: usize,
}

/// Progress of an update, in the order [`ScanStep`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateStep {
    Checking,
    Reading,
    Page(ScanProgress),
    CheckingMissing {
        done: usize,
        total: usize,
    },
    /// The scan is read completely and is being written to the library.
    Saving,
}

/// Why a saved profile is on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loaded {
    /// The first scan of a profile finished; it is saved now.
    First,
    /// Opened from the library, no request made.
    Opened,
    Updated(UpdateReport),
}

/// What the library knows about one file of the open profile; same order as `ProfileLoaded::files`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMark {
    pub downloaded: bool,
    /// "Download new only" would fetch it.
    pub pending: bool,
    pub saved_path: Option<PathBuf>,
}

/// A saved profile, ready to show.
#[derive(Clone, Debug)]
pub struct ProfileLoaded {
    pub how: Loaded,
    pub record: ProfileRecord,
    pub posts: Vec<PostRow>,
    pub files: Vec<FileEntry>,
    pub marks: Vec<FileMark>,
    pub links: Vec<Link>,
    /// Files whose size is about to be measured in the background (none for an opened profile).
    pub probing: usize,
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    Log(String),
    ScanStarted(ScanTarget),
    ScanProgress(ScanProgress),
    ScanFinished {
        creator: CreatorRef,
        posts: usize,
        files: Vec<FileEntry>,
        /// External links in the posts' text, in display order (`Link::id` is the index).
        links: Vec<Link>,
        /// Set when the scan was a single post.
        post: Option<PostRef>,
    },
    ScanFailed(String),
    SizingProgress {
        done: usize,
        total: usize,
    },
    FileSized {
        index: usize,
        size: Option<u64>,
        content_type: Option<String>,
    },
    /// The size probe got HTTP 404: the server no longer has this file (index into the scan's
    /// `files`). Such a file is excluded from downloads, like a 404 during a download.
    FileMissing {
        index: usize,
    },
    SizingFinished {
        skipped: bool,
    },
    LinkChecking {
        link: usize,
    },
    LinkResolved {
        link: usize,
        resolved: Resolved,
    },
    /// Nothing can be downloaded from the link. Permanent.
    LinkDead {
        link: usize,
        reason: DeadReason,
    },
    /// The check could not finish. `message` is `None` when the user skipped it (back to unchecked).
    LinkCheckFailed {
        link: usize,
        message: Option<String>,
    },
    CheckProgress {
        done: usize,
        total: usize,
    },
    CheckFinished {
        skipped: bool,
    },
    /// An external host asked us to slow down; only its requests stop for `secs`.
    HostCooldown {
        host: ExtHost,
        secs: u64,
    },
    /// Download was pressed while picked links still had to be checked; `DownloadStarted` (or
    /// `DownloadFinished` if nothing starts) follows once the checks end.
    DownloadWaiting,
    DownloadStarted {
        jobs: Vec<JobInfo>,
    },
    Download(engine::Event),
    DownloadFinished(engine::Summary),
    /// The saved profiles, most recently scanned first, or why the library could not be opened.
    /// Sent once at start and again whenever the list changes.
    Profiles(Result<Vec<ProfileRecord>, String>),
    UpdateStarted {
        kind: UpdateKind,
        creator: CreatorRef,
    },
    UpdateStep(UpdateStep),
    /// The update did not finish; the library is exactly as it was. `saving`: the scan was read
    /// completely and writing it failed.
    UpdateFailed {
        cause: String,
        saving: bool,
    },
    UpdateCancelled,
    ProfileLoaded(Box<ProfileLoaded>),
    ProfileRemoved {
        creator: CreatorRef,
    },
    /// The library changed after a download: which files are downloaded now.
    FileMarks(Vec<FileMark>),
}

/// Messages into the session loop (commands plus results from its own tasks).
enum Msg {
    Cmd(Command),
    ScanDone(Result<crate::api::ScanResult, Error>),
    Sized { index: usize, size: Option<u64>, content_type: Option<String> },
    Missing { index: usize },
    SizingDone { skipped: bool },
    Checked { link: usize, host: ExtHost, result: Result<Resolved, ResolveError> },
    Dl(engine::Event),
    DlDone(engine::Summary),
    LibScanDone { creator: CreatorRef, kind: UpdateKind, first: bool, result: Result<ScanOutcome, Error> },
}

#[derive(Clone)]
pub struct SessionHandle {
    tx: UnboundedSender<Msg>,
}

impl SessionHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(Msg::Cmd(cmd));
    }
}

/// Engine/retry knobs. `Default` is the production policy; tests shrink the waits.
#[derive(Clone, Debug, Default)]
pub struct Tuning {
    pub retry: RetryPolicy,
    /// Overrides the settings delay (tests only; the real minimum is enforced by `Settings`).
    pub delay_override: Option<std::time::Duration>,
}

/// Start the session thread. `notify` is called after every event so a GUI can request a repaint.
/// Without a library every scan is the plain scan of v0.2.0.
pub fn spawn(
    settings: Settings,
    hosts: Hosts,
    tuning: Tuning,
    notify: Arc<dyn Fn() + Send + Sync>,
) -> (SessionHandle, std_mpsc::Receiver<UiEvent>) {
    spawn_with_library(settings, hosts, tuning, notify, None)
}

/// [`spawn`] with the saved library: `Some(Ok(library))` saves every creator scan in it,
/// `Some(Err(why))` tells the front end the library could not be opened (the session then works
/// without one), `None` has no library at all.
pub fn spawn_with_library(
    settings: Settings,
    hosts: Hosts,
    tuning: Tuning,
    notify: Arc<dyn Fn() + Send + Sync>,
    library: Option<Result<Library, String>>,
) -> (SessionHandle, std_mpsc::Receiver<UiEvent>) {
    let (msg_tx, mut msg_rx) = unbounded_channel::<Msg>();
    let (ev_tx, ev_rx) = std_mpsc::channel::<UiEvent>();
    let handle = SessionHandle { tx: msg_tx.clone() };
    std::thread::Builder::new()
        .name("pawchive-session".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let mut s = Session::new(settings, hosts, tuning, msg_tx, ev_tx, notify);
                s.attach_library(library);
                while let Some(m) = msg_rx.recv().await {
                    s.handle(m);
                }
            });
        })
        .expect("spawn session thread");
    (handle, ev_rx)
}

struct Session {
    settings: Settings,
    hosts: Hosts,
    tuning: Tuning,
    client: Option<Client>,
    tx: UnboundedSender<Msg>,
    ev: std_mpsc::Sender<UiEvent>,
    notify: Arc<dyn Fn() + Send + Sync>,
    creator: Option<CreatorRef>,
    files: Vec<FileEntry>,
    scan: Option<Control>,
    sizing: Option<Control>,
    download: Option<Control>,
    last_jobs: Vec<Job>,
    failed: HashSet<u64>,
    ext: Option<ExtClient>,
    target: Option<ScanTarget>,
    links: Vec<Link>,
    resolved: HashMap<usize, Resolved>,
    dead: HashSet<usize>,
    check_queue: HashMap<ExtHost, VecDeque<usize>>,
    check_running: HashSet<ExtHost>,
    check_queued: HashSet<usize>,
    check_control: Control,
    check_total: usize,
    check_done: usize,
    check_skipped: bool,
    pending: Option<PendingDownload>,
    library: Option<Library>,
    /// The open creator is a saved profile: downloads are recorded in the library.
    saved_open: bool,
    /// Library file behind each job of the current download, by job id.
    targets: HashMap<u64, DownloadTarget>,
}

/// A download waiting for its picked links to be checked first.
struct PendingDownload {
    selection: Selection,
    custom: String,
    output: PathBuf,
    picks: Vec<LinkPick>,
    new_only: bool,
}

impl Session {
    fn new(
        settings: Settings,
        hosts: Hosts,
        tuning: Tuning,
        tx: UnboundedSender<Msg>,
        ev: std_mpsc::Sender<UiEvent>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let mut s = Session {
            settings: settings.clone().normalized(),
            hosts,
            tuning,
            client: None,
            tx,
            ev,
            notify,
            creator: None,
            files: Vec::new(),
            scan: None,
            sizing: None,
            download: None,
            last_jobs: Vec::new(),
            failed: HashSet::new(),
            ext: None,
            target: None,
            links: Vec::new(),
            resolved: HashMap::new(),
            dead: HashSet::new(),
            check_queue: HashMap::new(),
            check_running: HashSet::new(),
            check_queued: HashSet::new(),
            check_control: Control::new(),
            check_total: 0,
            check_done: 0,
            check_skipped: false,
            pending: None,
            library: None,
            saved_open: false,
            targets: HashMap::new(),
        };
        s.apply_settings(settings);
        s
    }

    fn emit(&self, e: UiEvent) {
        let _ = self.ev.send(e);
        (self.notify)();
    }

    fn log(&self, m: impl Into<String>) {
        self.emit(UiEvent::Log(m.into()));
    }

    fn apply_settings(&mut self, settings: Settings) {
        self.settings = settings.normalized();
        // Cookie contents stay in memory only; only the path is ever mentioned.
        let jar = match &self.settings.cookie_file {
            Some(p) => match CookieJar::load(p) {
                Ok(j) => {
                    self.log(format!("Loaded {} cookies from {} (kept in memory only)", j.len(), p.display()));
                    Some(j)
                }
                Err(e) => {
                    self.log(format!("Could not read cookie file: {e}"));
                    None
                }
            },
            None => None,
        };
        let delay = self.tuning.delay_override.unwrap_or_else(|| self.settings.delay());
        // Google Drive and Mediafire never see the cookie or the contact string (DAN-1609).
        self.ext = ExtClient::new(self.hosts.clone(), delay, self.tuning.retry.clone()).ok();
        match Http::new(&self.settings, jar) {
            Ok(http) => {
                self.client = Some(Client {
                    http,
                    gate: Gate::new(delay),
                    hosts: self.hosts.clone(),
                    policy: self.tuning.retry.clone(),
                });
            }
            Err(e) => {
                self.client = None;
                self.log(format!("Cannot set up the HTTP client: {e}"));
            }
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Cmd(c) => self.command(c),
            Msg::ScanDone(r) => {
                self.scan = None;
                match r {
                    Ok(res) => {
                        self.files = res.files.clone();
                        self.creator = Some(res.creator.clone());
                        self.links = res.links.clone();
                        self.emit(UiEvent::ScanFinished {
                            creator: res.creator,
                            posts: res.posts,
                            files: res.files,
                            links: res.links,
                            post: res.post,
                        });
                        self.start_sizing();
                    }
                    Err(Error::Cancelled) => self.emit(UiEvent::ScanFailed("Scan cancelled".into())),
                    Err(e) => self.emit(UiEvent::ScanFailed(e.to_string())),
                }
            }
            Msg::Sized { index, size, content_type } => {
                if let Some(f) = self.files.get_mut(index) {
                    f.apply_probe(size, content_type.clone());
                }
                self.emit(UiEvent::FileSized { index, size, content_type });
            }
            Msg::Missing { index } => {
                if let Some(f) = self.files.get_mut(index) {
                    f.missing = true;
                }
                self.emit(UiEvent::FileMissing { index });
            }
            Msg::Checked { link, host, result } => self.link_checked(link, host, result),
            Msg::SizingDone { skipped } => {
                self.sizing = None;
                self.emit(UiEvent::SizingFinished { skipped });
            }
            Msg::Dl(ev) => {
                self.record_download(&ev);
                match &ev {
                    // Only failures that a retry can fix; a 404 stays failed for good.
                    engine::Event::Failed { id, permanent: false, .. } => {
                        self.failed.insert(*id);
                    }
                    engine::Event::Done { id, .. } | engine::Event::Skipped { id } => {
                        self.failed.remove(id);
                    }
                    _ => {}
                }
                self.emit(UiEvent::Download(ev));
            }
            Msg::DlDone(summary) => {
                self.download = None;
                self.emit(UiEvent::DownloadFinished(summary));
                self.send_marks();
            }
            Msg::LibScanDone { creator, kind, first, result } => self.library_scan_done(creator, kind, first, result),
        }
    }

    fn command(&mut self, c: Command) {
        match c {
            Command::SetSettings(s) => self.apply_settings(s),
            Command::Scan { url } => self.start_scan(&url, UpdateKind::Update),
            Command::FullRescan { url } => self.start_scan(&url, UpdateKind::Full),
            Command::OpenProfile(creator) => self.open_profile(&creator),
            Command::RemoveProfile(creator) => self.remove_profile(&creator),
            Command::DownloadNew { selection, custom_extensions, output, links } => {
                self.start_download(selection, custom_extensions, output, links, true)
            }
            Command::SkipSizing => {
                if let Some(c) = &self.sizing {
                    c.cancel();
                }
            }
            Command::StartDownload { selection, custom_extensions, output, links } => {
                self.start_download(selection, custom_extensions, output, links, false)
            }
            Command::CheckLinks { links } => self.check_links(links, false),
            Command::SkipChecking => self.skip_checking(),
            Command::Pause => {
                if let Some(c) = &self.download {
                    c.pause();
                    self.log("Paused: no new files start; in-flight files are parked (.part kept)");
                }
            }
            Command::Resume => {
                if let Some(c) = &self.download {
                    c.resume();
                    self.log("Resumed");
                }
            }
            Command::Cancel => {
                if self.pending.take().is_some() {
                    self.log("Download cancelled before it started");
                    self.skip_checking();
                    self.emit(UiEvent::DownloadFinished(engine::Summary::default()));
                }
                if let Some(c) = self.download.as_ref().or(self.scan.as_ref()) {
                    c.cancel();
                }
            }
            Command::RetryFailed => self.retry(None),
            Command::RetryJobs(ids) => self.retry(Some(ids)),
        }
    }

    /// Re-run failed jobs: all of them, or only `only`.
    fn retry(&mut self, only: Option<Vec<u64>>) {
        if self.download.is_some() || self.scan.is_some() {
            return self.log("Busy: a scan or download is already running");
        }
        let wanted = |id: u64| only.as_ref().is_none_or(|ids| ids.contains(&id));
        let jobs: Vec<Job> =
            self.last_jobs.iter().filter(|j| self.failed.contains(&j.id) && wanted(j.id)).cloned().collect();
        if jobs.is_empty() {
            return self.log("No failed files to retry");
        }
        for j in &jobs {
            self.failed.remove(&j.id);
        }
        self.run_jobs(jobs);
    }

    fn start_scan(&mut self, url: &str, kind: UpdateKind) {
        if self.scan.is_some() || self.download.is_some() {
            self.log("Busy: wait for the current scan/download to finish or cancel it");
            return;
        }
        let target = match parse_scan_url(url) {
            Ok(t) => t,
            Err(e) => return self.emit(UiEvent::ScanFailed(e.to_string())),
        };
        // A creator link goes through the library when there is one; a post link never does.
        if self.library.is_some() && target.post_id.is_none() {
            return self.start_library_scan(target, kind);
        }
        self.saved_open = false;
        let Some(client) = self.client.clone() else {
            return self.emit(UiEvent::ScanFailed("HTTP client is not available; check the settings".into()));
        };
        if let Some(c) = self.sizing.take() {
            c.cancel();
        }
        self.files.clear();
        self.last_jobs.clear();
        self.failed.clear();
        self.reset_links();
        let control = Control::new();
        self.scan = Some(control.clone());
        self.target = Some(target.clone());
        self.emit(UiEvent::ScanStarted(target.clone()));
        let (tx, ev, notify) = (self.tx.clone(), self.ev.clone(), self.notify.clone());
        tokio::spawn(async move {
            let res = match &target.post_id {
                // A post link scans that post alone: one request, not the whole creator.
                Some(post_id) => client.scan_post(&target.creator, post_id, &control).await,
                None => {
                    client
                        .scan(&target.creator, &control, |p| {
                            let _ = ev.send(UiEvent::ScanProgress(p));
                            notify();
                        })
                        .await
                }
            };
            let _ = tx.send(Msg::ScanDone(res));
        });
    }

    /// Probe every downloadable file's size and Content-Type in the background.
    fn start_sizing(&mut self) {
        let all: Vec<usize> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.preview_only) // preview-only files would 404 on the full-size host
            .map(|(i, _)| i)
            .collect();
        self.start_sizing_for(all);
    }

    /// Probe these files (indexes into the scan's file list).
    fn start_sizing_for(&mut self, which: Vec<usize>) {
        let Some(client) = self.client.clone() else { return };
        let targets: Vec<(usize, String)> =
            which.into_iter().filter_map(|i| self.files.get(i).map(|f| (i, self.hosts.full_url(&f.path)))).collect();
        if targets.is_empty() {
            return self.emit(UiEvent::SizingFinished { skipped: false });
        }
        let control = Control::new();
        self.sizing = Some(control.clone());
        let (tx, ev, notify) = (self.tx.clone(), self.ev.clone(), self.notify.clone());
        tokio::spawn(async move {
            let total = targets.len();
            let mut skipped = false;
            for (n, (index, url)) in targets.into_iter().enumerate() {
                match probe(&client.http, &client.gate, &control, &client.policy, &url).await {
                    Ok(info) => {
                        let _ = tx.send(Msg::Sized { index, size: info.size, content_type: info.content_type });
                    }
                    Err(Error::Cancelled) => {
                        skipped = true;
                        break;
                    }
                    // Permanent: the original is gone (a download would 404 too).
                    Err(Error::NotFound(_)) => {
                        let _ = tx.send(Msg::Missing { index });
                    }
                    Err(e) => {
                        let _ = ev.send(UiEvent::Log(format!(
                            "Could not size a file ({e}); it will be sized when downloaded"
                        )));
                        notify();
                    }
                }
                let _ = ev.send(UiEvent::SizingProgress { done: n + 1, total });
                notify();
            }
            let _ = tx.send(Msg::SizingDone { skipped });
        });
    }

    /// Forget everything about the previous scan's links.
    fn reset_links(&mut self) {
        self.check_control.cancel();
        self.check_control = Control::new();
        self.links.clear();
        self.resolved.clear();
        self.dead.clear();
        self.check_queue.clear();
        self.check_running.clear();
        self.check_queued.clear();
        self.check_total = 0;
        self.check_done = 0;
        self.pending = None;
    }

    /// Queue links for checking (`front`: ahead of what is already waiting). One request at a time
    /// per host; Drive and Mediafire run side by side, each on its own clock.
    fn check_links(&mut self, ids: Vec<usize>, front: bool) {
        if self.download.is_some() || self.scan.is_some() {
            return self.log("Busy: links are not checked while a scan or download is running");
        }
        if self.ext.is_none() {
            return self.log("Cannot check links: the HTTP client is not available");
        }
        let idle = self.check_running.is_empty() && self.check_queue.values().all(VecDeque::is_empty);
        if idle {
            self.check_total = 0;
            self.check_done = 0;
            self.check_skipped = false;
        }
        let mut added = Vec::new();
        for id in ids {
            let Some(host) = self.links.get(id).and_then(external::ext_host_of) else { continue };
            if self.resolved.contains_key(&id) || self.dead.contains(&id) || self.check_queued.contains(&id) {
                continue;
            }
            self.check_queued.insert(id);
            added.push((host, id));
        }
        for (host, id) in added.iter().rev().filter(|_| front).chain(added.iter().filter(|_| !front)) {
            let q = self.check_queue.entry(*host).or_default();
            if front {
                q.push_front(*id);
            } else {
                q.push_back(*id);
            }
            self.check_total += 1;
        }
        if !added.is_empty() {
            self.emit(UiEvent::CheckProgress { done: self.check_done, total: self.check_total });
        }
        self.pump_checks();
    }

    fn pump_checks(&mut self) {
        let Some(ext) = self.ext.as_ref().map(ExtClient::for_checks) else { return };
        for host in [ExtHost::Drive, ExtHost::Mediafire] {
            if self.check_running.contains(&host) {
                continue;
            }
            let Some(id) = self.check_queue.get_mut(&host).and_then(VecDeque::pop_front) else { continue };
            self.check_running.insert(host);
            self.emit(UiEvent::LinkChecking { link: id });
            let link = self.links[id].clone();
            let (tx, ev, notify, control, ext) =
                (self.tx.clone(), self.ev.clone(), self.notify.clone(), self.check_control.clone(), ext.clone());
            tokio::spawn(async move {
                let announce = |n: Notice| {
                    let Notice::Cooldown { host, secs } = n;
                    let _ = ev.send(UiEvent::HostCooldown { host, secs });
                    notify();
                };
                let result = external::resolve(&ext, &link, &control, &announce).await;
                let _ = tx.send(Msg::Checked { link: id, host, result });
            });
        }
    }

    fn link_checked(&mut self, link: usize, host: ExtHost, result: Result<Resolved, ResolveError>) {
        self.check_running.remove(&host);
        self.check_queued.remove(&link);
        self.check_done += 1;
        match result {
            Ok(res) => {
                if res.partial() {
                    self.log(format!(
                        "{} folder “{}”: listing may be incomplete ({} files found)",
                        host.name(),
                        res.name,
                        res.file_count()
                    ));
                }
                self.resolved.insert(link, res.clone());
                self.emit(UiEvent::LinkResolved { link, resolved: res });
            }
            Err(ResolveError::Dead(reason)) => {
                self.dead.insert(link);
                self.emit(UiEvent::LinkDead { link, reason });
            }
            Err(ResolveError::Transient { message, .. }) => {
                self.log(format!("Could not check a {} link ({message})", host.name()));
                self.emit(UiEvent::LinkCheckFailed { link, message: Some(message) });
            }
            Err(ResolveError::Cancelled) => self.emit(UiEvent::LinkCheckFailed { link, message: None }),
        }
        self.emit(UiEvent::CheckProgress { done: self.check_done, total: self.check_total });
        self.pump_checks();
        if self.check_running.is_empty() && self.check_queue.values().all(VecDeque::is_empty) {
            self.checks_idle();
        }
    }

    /// Nothing left to check: report it, and start a download that was waiting for the checks.
    fn checks_idle(&mut self) {
        let skipped = std::mem::take(&mut self.check_skipped);
        self.emit(UiEvent::CheckFinished { skipped });
        if let Some(p) = self.pending.take() {
            if skipped {
                self.log("Download not started: link checking was skipped");
                self.emit(UiEvent::DownloadFinished(engine::Summary::default()));
            } else {
                self.finish_start_download(p);
            }
        }
    }

    /// Drop the checks that have not started; each row goes back to "Not checked".
    fn drain_check_queue(&mut self) {
        for q in self.check_queue.values_mut() {
            for id in q.drain(..) {
                self.check_queued.remove(&id);
                self.check_total = self.check_total.saturating_sub(1);
                let _ = self.ev.send(UiEvent::LinkCheckFailed { link: id, message: None });
            }
        }
    }

    fn skip_checking(&mut self) {
        if self.check_running.is_empty() && self.check_queue.values().all(VecDeque::is_empty) {
            return;
        }
        self.check_skipped = true;
        self.drain_check_queue();
        // Stop the request in flight too; its result arrives as `Cancelled`.
        self.check_control.cancel();
        self.check_control = Control::new();
        if self.check_running.is_empty() {
            self.checks_idle();
        }
        (self.notify)();
    }

    fn start_download(
        &mut self,
        selection: Selection,
        custom: String,
        output: PathBuf,
        picks: Vec<LinkPick>,
        new_only: bool,
    ) {
        if self.download.is_some() || self.scan.is_some() || self.pending.is_some() {
            return self.log("Busy: a scan or download is already running");
        }
        if self.creator.is_none() {
            return self.log("Scan a creator first");
        }
        // Downloads and probes share the one request budget; downloads don't need sizes.
        if let Some(c) = self.sizing.take() {
            c.cancel();
            self.log("Stopped size probing to keep requests polite while downloading");
        }
        let missing: Vec<usize> = picks
            .iter()
            .map(|p| p.link)
            .filter(|id| !self.resolved.contains_key(id) && !self.dead.contains(id))
            .collect();
        let pending = PendingDownload { selection, custom, output, picks, new_only };
        if missing.is_empty() && self.check_running.is_empty() {
            return self.finish_start_download(pending);
        }
        // Picked links that were never checked are checked first. Other queued checks wait for the
        // next time: a download should not sit behind a long "Check links" run.
        self.drain_check_queue();
        self.pending = Some(pending);
        self.emit(UiEvent::DownloadWaiting);
        self.check_links(missing, true);
        if self.check_running.is_empty() && self.check_queue.values().all(VecDeque::is_empty) {
            self.checks_idle();
        }
    }

    fn finish_start_download(&mut self, p: PendingDownload) {
        let Some(creator) = self.creator.clone() else { return };
        let asked = p.picks.len();
        let picks: Vec<LinkPick> = p.picks.into_iter().filter(|k| self.resolved.contains_key(&k.link)).collect();
        if picks.len() < asked {
            let n = asked - picks.len();
            let (s, were) = if n == 1 { ("", "was") } else { ("s", "were") };
            self.log(format!(
                "{n} picked link{s} could not be checked (or {were} dead) and {were} left out of this download"
            ));
        }
        if self.saved_open && self.library.is_some() {
            return self.finish_library_download(creator, p.selection, p.custom, p.output, picks, p.new_only);
        }
        let plan = plan_all(
            &creator,
            &self.files,
            &p.selection,
            &CustomExts::parse(&p.custom),
            &p.output,
            &self.hosts,
            Some(LinkPlan { links: &self.links, resolved: &self.resolved, picks: &picks }),
        );
        if plan.is_empty() {
            self.log("Nothing selected to download");
            return self.emit(UiEvent::DownloadFinished(engine::Summary::default()));
        }
        let jobs: Vec<Job> = plan.iter().map(|x| x.job.clone()).collect();
        self.last_jobs = jobs.clone();
        self.failed.clear();
        self.emit(UiEvent::DownloadStarted {
            jobs: plan
                .iter()
                .map(|x| JobInfo {
                    id: x.job.id,
                    label: x.job.label.clone(),
                    expected_size: x.job.expected_size,
                    origin: x.origin,
                    renamed: x.renamed,
                })
                .collect(),
        });
        self.run_jobs(jobs);
    }

    fn run_jobs(&mut self, jobs: Vec<Job>) {
        let Some(client) = self.client.clone() else { return };
        let control = Control::new();
        self.download = Some(control.clone());
        let mut engine = Engine::new(
            client.http.clone(),
            client.gate.clone(),
            EngineConfig { concurrency: self.settings.concurrency, retry: self.tuning.retry.clone() },
        );
        if let Some(ext) = &self.ext {
            engine = engine.with_external(ext.clone());
        }
        if self.saved_open {
            // A saved profile never has a file replaced; what is there stays (DAN-1655).
            engine = engine.with_no_replace();
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let tx2 = tx.clone();
            let summary = engine
                .run(jobs, control, move |e| {
                    let _ = tx2.send(Msg::Dl(e));
                })
                .await;
            let _ = tx.send(Msg::DlDone(summary));
        });
    }
}

// ---- the saved library -----------------------------------------------------------------------

/// Pages of 50 a saved profile's post list takes: the estimate behind "page 14 of about 29".
pub fn est_pages(saved_posts: usize) -> usize {
    saved_posts.div_ceil(crate::api::PAGE_SIZE)
}

/// Which files of a freshly shown profile get their size measured in the background.
enum Probe {
    Nothing,
    All,
    PostsOf(HashSet<String>),
}

impl Session {
    fn attach_library(&mut self, library: Option<Result<Library, String>>) {
        match library {
            None => {}
            Some(Err(why)) => {
                self.emit(UiEvent::Profiles(Err(why)));
            }
            Some(Ok(lib)) => {
                self.library = Some(lib);
                self.send_profiles();
            }
        }
    }

    fn send_profiles(&self) {
        let Some(lib) = &self.library else { return };
        match lib.profiles() {
            Ok(list) => self.emit(UiEvent::Profiles(Ok(list))),
            Err(e) => self.log(format!("Could not read the saved profiles: {e}")),
        }
    }

    /// A creator scan with a library: the first scan of a profile, an update, or a full rescan.
    fn start_library_scan(&mut self, target: ScanTarget, kind: UpdateKind) {
        let (Some(lib), Some(client)) = (self.library.clone(), self.client.clone()) else {
            return self.emit(UiEvent::ScanFailed("HTTP client is not available; check the settings".into()));
        };
        let creator = target.creator.clone();
        let saved = match lib.profile(&creator) {
            Ok(s) => s,
            Err(e) => return self.emit(UiEvent::ScanFailed(format!("Could not read the library: {e}"))),
        };
        let first = saved.is_none();
        if let Some(c) = self.sizing.take() {
            c.cancel();
        }
        if first {
            self.files.clear();
            self.links.clear();
            self.saved_open = false;
        } else if self.creator.as_ref() != Some(&creator) || !self.saved_open {
            // Updating from the saved list: show the saved copy first, then update it.
            self.show_saved(&creator, Loaded::Opened, Probe::Nothing);
        }
        self.last_jobs.clear();
        self.failed.clear();
        self.targets.clear();
        self.reset_links();
        let control = Control::new();
        self.scan = Some(control.clone());
        self.target = Some(target.clone());
        if first {
            self.emit(UiEvent::ScanStarted(target));
        } else {
            self.emit(UiEvent::UpdateStarted { kind, creator: creator.clone() });
        }
        let mode = if kind == UpdateKind::Full { ScanMode::Full } else { ScanMode::Update };
        let (tx, ev, notify) = (self.tx.clone(), self.ev.clone(), self.notify.clone());
        tokio::spawn(async move {
            let result = lib
                .scan(&client, &creator, mode, &control, |step| {
                    let event = match (first, step) {
                        (true, ScanStep::Page(p)) => Some(UiEvent::ScanProgress(p)),
                        (true, _) => None,
                        (false, ScanStep::Checking) => Some(UiEvent::UpdateStep(UpdateStep::Checking)),
                        (false, ScanStep::Reading) => Some(UiEvent::UpdateStep(UpdateStep::Reading)),
                        (false, ScanStep::Page(p)) => Some(UiEvent::UpdateStep(UpdateStep::Page(p))),
                        (false, ScanStep::Missing { done, total }) => {
                            Some(UiEvent::UpdateStep(UpdateStep::CheckingMissing { done, total }))
                        }
                        (false, ScanStep::Saving) => Some(UiEvent::UpdateStep(UpdateStep::Saving)),
                    };
                    if let Some(e) = event {
                        let _ = ev.send(e);
                        notify();
                    }
                })
                .await;
            let _ = tx.send(Msg::LibScanDone { creator, kind, first, result });
        });
    }

    fn library_scan_done(
        &mut self,
        creator: CreatorRef,
        kind: UpdateKind,
        first: bool,
        result: Result<ScanOutcome, Error>,
    ) {
        self.scan = None;
        match result {
            Ok(out) => {
                let report = UpdateReport {
                    kind,
                    unchanged: out.unchanged,
                    summary: UpdateSummary {
                        new_posts: out.new_posts,
                        updated_posts: out.updated_posts,
                        new_files: out.new_files,
                        gone_posts: out.gone_posts,
                    },
                    posts_seen: out.posts_seen,
                    requests: out.requests,
                };
                let how = if first { Loaded::First } else { Loaded::Updated(report) };
                // A first scan measures everything; an update only the files of the posts it found
                // new or changed (an unchanged profile costs its one request and nothing more).
                let probe = if first {
                    Probe::All
                } else {
                    Probe::PostsOf(out.changes.iter().map(|c| c.post_id.clone()).collect())
                };
                if !self.show_saved(&creator, how, probe) {
                    self.emit(UiEvent::ScanFailed("The scan finished but the library could not be read back".into()));
                }
                self.send_profiles();
            }
            Err(Error::Cancelled) if first => self.emit(UiEvent::ScanFailed("Scan cancelled".into())),
            Err(Error::Cancelled) => self.emit(UiEvent::UpdateCancelled),
            Err(e) if first => self.emit(UiEvent::ScanFailed(e.to_string())),
            Err(e) => {
                let saving = matches!(e, Error::Db(_));
                self.emit(UiEvent::UpdateFailed { cause: e.to_string(), saving });
            }
        }
    }

    /// Put the saved profile on screen: the session's files and links become the library's, and the
    /// front end gets everything it draws. `probe`: which files get their size measured in the background.
    fn show_saved(&mut self, creator: &CreatorRef, how: Loaded, probe: Probe) -> bool {
        let Some(lib) = self.library.clone() else { return false };
        let loaded = match lib.load(creator) {
            Ok(Some(l)) => l,
            Ok(None) => return false,
            Err(e) => {
                self.log(format!("Could not read the library: {e}"));
                return false;
            }
        };
        // Sizes and Content-Types measured earlier in this run carry over; they are not saved.
        let known: HashMap<(String, String), (Option<u64>, Option<String>)> = if self.creator.as_ref() == Some(creator)
        {
            self.files
                .iter()
                .filter(|f| f.size.is_some() || f.content_type.is_some())
                .map(|f| ((f.post_id.clone(), f.path.clone()), (f.size, f.content_type.clone())))
                .collect()
        } else {
            HashMap::new()
        };
        let LoadedProfile { record, posts, files, links } = loaded;
        let mut entries = Vec::with_capacity(files.len());
        let mut marks = Vec::with_capacity(files.len());
        for f in files {
            let mut e = f.entry;
            if let Some((size, ct)) = known.get(&(e.post_id.clone(), e.path.clone())) {
                e.apply_probe(*size, ct.clone());
            }
            marks.push(FileMark { downloaded: f.downloaded, pending: f.pending, saved_path: f.saved_path });
            entries.push(e);
        }
        self.reset_links();
        self.creator = Some(creator.clone());
        self.files = entries.clone();
        self.links = links.clone();
        self.saved_open = true;
        self.last_jobs.clear();
        self.failed.clear();
        self.targets.clear();
        let to_probe: Vec<usize> = self
            .files
            .iter()
            .enumerate()
            .filter(|(i, f)| {
                !f.missing
                    && f.size.is_none()
                    && !marks[*i].downloaded
                    && match &probe {
                        Probe::Nothing => false,
                        Probe::All => true,
                        Probe::PostsOf(ids) => ids.contains(&f.post_id),
                    }
            })
            .map(|(i, _)| i)
            .collect();
        self.emit(UiEvent::ProfileLoaded(Box::new(ProfileLoaded {
            how,
            record,
            posts,
            files: entries,
            marks,
            links,
            probing: to_probe.len(),
        })));
        if !to_probe.is_empty() {
            self.start_sizing_for(to_probe);
        }
        true
    }

    fn open_profile(&mut self, creator: &CreatorRef) {
        if self.scan.is_some() || self.download.is_some() {
            return self.log("Busy: wait for the current scan/download to finish or cancel it");
        }
        if self.library.is_none() {
            return self.log("Cannot open a saved profile: there is no library");
        }
        if let Some(c) = self.sizing.take() {
            c.cancel();
        }
        if !self.show_saved(creator, Loaded::Opened, Probe::Nothing) {
            self.log(format!("Could not open {}/{}: it is not in the library", creator.service.as_str(), creator.id));
        }
    }

    fn remove_profile(&mut self, creator: &CreatorRef) {
        if self.scan.is_some() || self.download.is_some() {
            return self.log("Busy: wait for the current scan/download to finish or cancel it");
        }
        let Some(lib) = self.library.clone() else { return };
        match lib.remove_profile(creator) {
            Ok(_) => {
                if self.creator.as_ref() == Some(creator) {
                    self.creator = None;
                    self.files.clear();
                    self.links.clear();
                    self.saved_open = false;
                    self.reset_links();
                }
                self.emit(UiEvent::ProfileRemoved { creator: creator.clone() });
                self.send_profiles();
            }
            Err(e) => self.log(format!("Could not remove the profile: {e}")),
        }
    }

    /// Build and start a download of a saved profile: the library's files (all selected, or just the
    /// new ones), then thumbnails and picked links as before. Nothing is ever replaced.
    fn finish_library_download(
        &mut self,
        creator: CreatorRef,
        selection: Selection,
        custom: String,
        output: PathBuf,
        picks: Vec<LinkPick>,
        new_only: bool,
    ) {
        let Some(lib) = self.library.clone() else { return };
        let custom = CustomExts::parse(&custom);
        let keys: HashSet<(String, String)> = self
            .files
            .iter()
            .filter(|f| !f.missing && selection.categories.contains(&category_of(f, &custom)))
            .map(|f| (f.post_id.clone(), f.path.clone()))
            .collect();
        let candidates = match lib.download_candidates(&creator, &keys, new_only) {
            Ok(c) => c,
            Err(e) => {
                self.log(format!("Could not read the library: {e}"));
                return self.emit(UiEvent::DownloadFinished(engine::Summary::default()));
            }
        };
        let mut plan = match lib.plan_download(&creator, &output, candidates) {
            Ok(p) => p,
            Err(e) => {
                self.log(format!("Could not plan the download: {e}"));
                return self.emit(UiEvent::DownloadFinished(engine::Summary::default()));
            }
        };
        let sizes: HashMap<(&str, &str), Option<u64>> =
            self.files.iter().map(|f| ((f.post_id.as_str(), f.path.as_str()), f.size)).collect();
        for j in &mut plan.jobs {
            if let Some(t) = plan.targets.get(&j.id) {
                j.url = self.hosts.full_url(&t.path);
                j.expected_size = sizes.get(&(t.post_id.as_str(), t.path.as_str())).copied().flatten();
            }
        }
        let renamed: HashSet<u64> = plan
            .jobs
            .iter()
            .filter(|j| {
                let leaf = j.dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                plan.targets.get(&j.id).is_some_and(|t| {
                    self.files.iter().any(|f| {
                        f.post_id == t.post_id
                            && f.path == t.path
                            && sanitize_component(&f.name, MAX_COMPONENT_BYTES) != leaf
                    })
                })
            })
            .map(|j| j.id)
            .collect();
        // Thumbnails (not for "new only") and picked links are planned as before.
        let extra_sel =
            Selection { categories: HashSet::new(), include_preview: selection.include_preview && !new_only };
        let extra = plan_all(
            &creator,
            &self.files,
            &extra_sel,
            &custom,
            &output,
            &self.hosts,
            Some(LinkPlan { links: &self.links, resolved: &self.resolved, picks: &picks }),
        );
        let mut allocators: HashMap<PathBuf, NameAllocator> = HashMap::new();
        for j in &plan.jobs {
            if let (Some(dir), Some(leaf)) = (j.dest.parent(), j.dest.file_name()) {
                allocators.entry(dir.to_path_buf()).or_default().allocate(&leaf.to_string_lossy());
            }
        }
        let mut jobs = plan.jobs;
        let mut infos: Vec<JobInfo> = jobs
            .iter()
            .map(|j| JobInfo {
                id: j.id,
                label: j.label.clone(),
                expected_size: j.expected_size,
                origin: JobOrigin::Pawchive,
                renamed: renamed.contains(&j.id),
            })
            .collect();
        for x in extra {
            let mut job = x.job;
            let mut was_renamed = x.renamed;
            // A thumbnail or link file that would land on a name the library's files took gets a new one.
            if let (Some(dir), Some(leaf)) = (job.dest.parent().map(PathBuf::from), job.dest.file_name()) {
                if let Some(alloc) = allocators.get_mut(&dir) {
                    let wanted = leaf.to_string_lossy().into_owned();
                    let got = alloc.allocate(&wanted);
                    if got != wanted {
                        job.dest = dir.join(&got);
                        if let Some((head, _)) = job.label.rsplit_once('/') {
                            job.label = format!("{head}/{got}");
                        }
                        was_renamed = true;
                    }
                }
            }
            job.id = jobs.len() as u64;
            infos.push(JobInfo {
                id: job.id,
                label: job.label.clone(),
                expected_size: job.expected_size,
                origin: x.origin,
                renamed: was_renamed,
            });
            jobs.push(job);
        }
        if jobs.is_empty() {
            self.log("Nothing selected to download");
            return self.emit(UiEvent::DownloadFinished(engine::Summary::default()));
        }
        self.targets = plan.targets;
        self.last_jobs = jobs.clone();
        self.failed.clear();
        self.emit(UiEvent::DownloadStarted { jobs: infos });
        self.run_jobs(jobs);
    }

    /// A file of a saved profile finished: record it in the library.
    fn record_download(&mut self, ev: &engine::Event) {
        let engine::Event::Done { id, .. } = ev else { return };
        let (Some(lib), Some(creator), Some(target)) = (&self.library, &self.creator, self.targets.get(id)) else {
            return;
        };
        let Some(job) = self.last_jobs.iter().find(|j| j.id == *id) else { return };
        if let Err(e) = lib.record_done(creator, target, &job.dest) {
            let msg = format!("Could not record {} in the library: {e}", job.dest.display());
            self.log(msg);
        }
    }

    /// After a download: which files the library has as downloaded now.
    fn send_marks(&mut self) {
        let (Some(lib), Some(creator)) = (self.library.clone(), self.creator.clone()) else { return };
        if !self.saved_open {
            return;
        }
        if let Ok(Some(l)) = lib.load(&creator) {
            let marks: Vec<FileMark> = l
                .files
                .into_iter()
                .map(|f| FileMark { downloaded: f.downloaded, pending: f.pending, saved_path: f.saved_path })
                .collect();
            if marks.len() == self.files.len() {
                self.emit(UiEvent::FileMarks(marks));
            }
        }
    }
}
