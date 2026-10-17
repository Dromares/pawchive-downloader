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
use crate::external::{self, DeadReason, ExtClient, ExtHost, Notice, Resolved, ResolveError};
use crate::gate::Gate;
use crate::http::{Http, RetryPolicy};
use crate::links::{Link, PostRef};
use crate::model::FileEntry;
use crate::plan::{plan_all, JobOrigin, LinkPick, LinkPlan, Selection};
use crate::probe::probe;
use crate::settings::Settings;

#[derive(Clone, Debug)]
pub enum Command {
    Scan {
        url: String,
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
    DownloadStarted {
        jobs: Vec<JobInfo>,
    },
    Download(engine::Event),
    DownloadFinished(engine::Summary),
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
pub fn spawn(
    settings: Settings,
    hosts: Hosts,
    tuning: Tuning,
    notify: Arc<dyn Fn() + Send + Sync>,
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
}

/// A download waiting for its picked links to be checked first.
struct PendingDownload {
    selection: Selection,
    custom: String,
    output: PathBuf,
    picks: Vec<LinkPick>,
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
            }
        }
    }

    fn command(&mut self, c: Command) {
        match c {
            Command::SetSettings(s) => self.apply_settings(s),
            Command::Scan { url } => self.start_scan(&url),
            Command::SkipSizing => {
                if let Some(c) = &self.sizing {
                    c.cancel();
                }
            }
            Command::StartDownload { selection, custom_extensions, output, links } => {
                self.start_download(selection, custom_extensions, output, links)
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

    fn start_scan(&mut self, url: &str) {
        if self.scan.is_some() || self.download.is_some() {
            self.log("Busy: wait for the current scan/download to finish or cancel it");
            return;
        }
        let target = match parse_scan_url(url) {
            Ok(t) => t,
            Err(e) => return self.emit(UiEvent::ScanFailed(e.to_string())),
        };
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
        let Some(client) = self.client.clone() else { return };
        let targets: Vec<(usize, String)> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.preview_only) // preview-only files would 404 on the full-size host
            .map(|(i, f)| (i, self.hosts.full_url(&f.path)))
            .collect();
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
        let Some(ext) = self.ext.clone() else { return };
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
        for (_, q) in self.check_queue.iter_mut() {
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

    fn start_download(&mut self, selection: Selection, custom: String, output: PathBuf, picks: Vec<LinkPick>) {
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
        let pending = PendingDownload { selection, custom, output, picks };
        if missing.is_empty() && self.check_running.is_empty() {
            return self.finish_start_download(pending);
        }
        // Picked links that were never checked are checked first. Other queued checks wait for the
        // next time: a download should not sit behind a long "Check links" run.
        self.drain_check_queue();
        self.pending = Some(pending);
        self.check_links(missing, true);
        if self.check_running.is_empty() && self.check_queue.values().all(VecDeque::is_empty) {
            self.checks_idle();
        }
    }

    fn finish_start_download(&mut self, p: PendingDownload) {
        let Some(creator) = self.creator.clone() else { return };
        let picks: Vec<LinkPick> = p.picks.into_iter().filter(|k| self.resolved.contains_key(&k.link)).collect();
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
