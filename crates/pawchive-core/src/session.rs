//! The command/event layer between a front end and the network code. A front end sends
//! [`Command`]s and drains [`UiEvent`]s; everything slow runs on a tokio runtime on its own
//! thread, so the UI thread never blocks and core stays free of GUI dependencies.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::api::{Client, Hosts, ScanProgress};
use crate::classify::CustomExts;
use crate::control::Control;
use crate::cookies::CookieJar;
use crate::creator::{parse_creator_url, CreatorRef};
use crate::engine::{self, Engine, EngineConfig, Job};
use crate::error::Error;
use crate::gate::Gate;
use crate::http::{Http, RetryPolicy};
use crate::model::FileEntry;
use crate::plan::{plan_downloads, Selection};
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
    },
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
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    Log(String),
    ScanStarted(CreatorRef),
    ScanProgress(ScanProgress),
    ScanFinished {
        creator: CreatorRef,
        posts: usize,
        files: Vec<FileEntry>,
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
        match Http::new(&self.settings, jar) {
            Ok(http) => {
                let delay = self.tuning.delay_override.unwrap_or_else(|| self.settings.delay());
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
                        self.emit(UiEvent::ScanFinished { creator: res.creator, posts: res.posts, files: res.files });
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
            Command::StartDownload { selection, custom_extensions, output } => {
                self.start_download(&selection, &custom_extensions, &output)
            }
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
        let creator = match parse_creator_url(url) {
            Ok(c) => c,
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
        let control = Control::new();
        self.scan = Some(control.clone());
        self.emit(UiEvent::ScanStarted(creator.clone()));
        let (tx, ev, notify) = (self.tx.clone(), self.ev.clone(), self.notify.clone());
        tokio::spawn(async move {
            let res = client
                .scan(&creator, &control, |p| {
                    let _ = ev.send(UiEvent::ScanProgress(p));
                    notify();
                })
                .await;
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

    fn start_download(&mut self, sel: &Selection, custom: &str, output: &std::path::Path) {
        if self.download.is_some() || self.scan.is_some() {
            return self.log("Busy: a scan or download is already running");
        }
        let Some(creator) = self.creator.clone() else {
            return self.log("Scan a creator first");
        };
        let jobs = plan_downloads(&creator, &self.files, sel, &CustomExts::parse(custom), output, &self.hosts);
        if jobs.is_empty() {
            self.log("Nothing selected to download");
            return self.emit(UiEvent::DownloadFinished(engine::Summary::default()));
        }
        // Downloads and probes share the one request budget; downloads don't need sizes.
        if let Some(c) = self.sizing.take() {
            c.cancel();
            self.log("Stopped size probing to keep requests polite while downloading");
        }
        self.last_jobs = jobs.clone();
        self.failed.clear();
        self.emit(UiEvent::DownloadStarted {
            jobs: jobs
                .iter()
                .map(|j| JobInfo { id: j.id, label: j.label.clone(), expected_size: j.expected_size })
                .collect(),
        });
        self.run_jobs(jobs);
    }

    fn run_jobs(&mut self, jobs: Vec<Job>) {
        let Some(client) = self.client.clone() else { return };
        let control = Control::new();
        self.download = Some(control.clone());
        let engine = Engine::new(
            client.http.clone(),
            client.gate.clone(),
            EngineConfig { concurrency: self.settings.concurrency, retry: self.tuning.retry.clone() },
        );
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
