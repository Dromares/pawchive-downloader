//! Pure view-model: everything the screen shows, updated from session events and user actions.
//! No egui here, so the state machine from `ui-spec` section 5 is unit-testable.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pawchive_core::api::ScanProgress;
use pawchive_core::creator::{CreatorRef, ScanTarget};
use pawchive_core::engine::{Event, Summary};
use pawchive_core::external::ExtHost;
use pawchive_core::links::{self, PostRef};
use pawchive_core::model::FileEntry;
use pawchive_core::plan::JobOrigin;
use pawchive_core::session::UiEvent;

use crate::format::human_size;
use crate::links_state::{Check, LinkFilter, LinkRow};

const LOG_CAP: usize = 1000;
/// Overall speed is averaged over this window; ETA needs this much run time first.
const SPEED_WINDOW: Duration = Duration::from_secs(5);
const ETA_AFTER: Duration = Duration::from_secs(15);
/// A scan with no new page for this long shows "Still waiting for pawchive.pw…".
const SCAN_STALL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Empty,
    Scanning,
    ScanError,
    Ready,
    Downloading,
    Paused,
    Cancelled,
    Finished,
}

impl Phase {
    /// A download exists and has not ended (the quit dialog guards these).
    pub fn running(self) -> bool {
        matches!(self, Phase::Downloading | Phase::Paused)
    }

    pub fn busy(self) -> bool {
        self == Phase::Scanning || self.running()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    Queued,
    Active,
    Retrying {
        attempt: u32,
        until: Instant,
    },
    Done,
    Skipped,
    Failed {
        error: String,
        permanent: bool,
    },
    /// In flight when the run was cancelled; the `.part` file is kept.
    Stopped,
}

#[derive(Clone, Debug)]
pub struct Row {
    /// Post folder, shown in the Post column.
    pub post: String,
    pub name: String,
    pub state: RowState,
    pub done: u64,
    pub total: Option<u64>,
    /// A Pawchive file, or a file of an external link. Link files are listed in the External links
    /// tab, never in the Files tab, but they are jobs like any other and count in the totals.
    pub origin: JobOrigin,
    /// The name on disk differs from the file's own (`4 (2).png`).
    pub renamed: bool,
    /// What the server said about a link file (never the link text): `Content-Type` and the real name.
    pub content_type: Option<String>,
    pub real_name: Option<String>,
}

impl Row {
    pub fn new(post: impl Into<String>, name: impl Into<String>, total: Option<u64>) -> Self {
        Row {
            post: post.into(),
            name: name.into(),
            state: RowState::Queued,
            done: 0,
            total,
            origin: JobOrigin::Pawchive,
            renamed: false,
            content_type: None,
            real_name: None,
        }
    }

    pub fn is_pawchive(&self) -> bool {
        self.origin == JobOrigin::Pawchive
    }

    pub fn percent(&self) -> Option<f32> {
        match self.total {
            Some(t) if t > 0 => Some((self.done as f32 / t as f32).clamp(0.0, 1.0)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sizing {
    Running { done: usize, total: usize },
    Finished { skipped: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub unix: u64,
    pub level: Level,
    pub text: String,
}

/// Why the last scan stopped, with what had been found by then.
#[derive(Clone, Debug)]
pub struct ScanFailure {
    pub message: String,
    pub progress: ScanProgress,
}

pub struct Model {
    pub phase: Phase,
    pub creator: Option<CreatorRef>,
    /// What the pasted link asked for: a whole creator, or one post (`post_id`).
    pub scan_target: Option<ScanTarget>,
    /// Set after a single-post scan.
    pub scan_post: Option<PostRef>,
    pub posts: usize,
    pub scan_progress: ScanProgress,
    pub scan_started: Option<Instant>,
    scan_last_progress: Option<Instant>,
    pub scan_failure: Option<ScanFailure>,
    pub files: Vec<FileEntry>,
    /// Bumped whenever `files` changes, so cached summaries know when to refresh.
    pub files_version: u64,
    /// External links of the scan, with their check state and ticks.
    pub links: Vec<LinkRow>,
    pub links_posts: usize,
    pub link_filter: LinkFilter,
    /// `Checking links · done of total`, while the session checks links.
    pub checking: Option<(usize, usize)>,
    /// Download was pressed and is waiting for the picked links to be checked first.
    pub awaiting_checks: bool,
    /// External hosts that asked us to slow down, and until when.
    pub cooldowns: HashMap<ExtHost, Instant>,
    /// Link row index (in `rows`) of each link file's job: `(link, child)` -> `rows` index.
    pub link_jobs: HashMap<(usize, Option<usize>), usize>,
    pub sizing: Sizing,
    pub rows: Vec<Row>,
    pub run_started: Option<Instant>,
    run_elapsed: Option<Duration>,
    /// End of the global 60 s cool-down after a bot page.
    pub slow_until: Option<Instant>,
    pub cancel_requested: bool,
    pub summary: Option<Summary>,
    pub log: VecDeque<LogLine>,
    samples: VecDeque<(Instant, u64)>,
    finished_flag: bool,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            phase: Phase::Empty,
            creator: None,
            scan_target: None,
            scan_post: None,
            posts: 0,
            scan_progress: ScanProgress::default(),
            scan_started: None,
            scan_last_progress: None,
            scan_failure: None,
            files: Vec::new(),
            files_version: 0,
            links: Vec::new(),
            links_posts: 0,
            link_filter: LinkFilter::All,
            checking: None,
            awaiting_checks: false,
            cooldowns: HashMap::new(),
            link_jobs: HashMap::new(),
            sizing: Sizing::Finished { skipped: false },
            rows: Vec::new(),
            run_started: None,
            run_elapsed: None,
            slow_until: None,
            cancel_requested: false,
            summary: None,
            log: VecDeque::new(),
            samples: VecDeque::new(),
            finished_flag: false,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Model {
    pub fn push_log(&mut self, level: Level, text: impl Into<String>) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(LogLine { unix: unix_now(), level, text: text.into() });
    }

    // ---- derived numbers ------------------------------------------------------------------

    pub fn count(&self, pred: impl Fn(&RowState) -> bool) -> usize {
        self.rows.iter().filter(|r| pred(&r.state)).count()
    }

    /// Rows of the run that are Pawchive files (the Files tab and its chips).
    pub fn pawchive_rows(&self) -> impl Iterator<Item = (usize, &Row)> {
        self.rows.iter().enumerate().filter(|(_, r)| r.is_pawchive())
    }

    /// Selected link files in the run.
    pub fn link_file_rows(&self) -> usize {
        self.rows.iter().filter(|r| !r.is_pawchive()).count()
    }

    /// How a link's download went, for the export (`None` before a run, or when it was not picked).
    pub fn link_outcome(&self, link: usize) -> Option<crate::links_state::RunOutcome> {
        use crate::links_state::RunOutcome;
        let mut any = false;
        let (mut failed, mut quota, mut open) = (false, false, false);
        for r in self.rows.iter().filter(|r| matches!(r.origin, JobOrigin::Link { link: l, .. } if l == link)) {
            any = true;
            match &r.state {
                RowState::Done | RowState::Skipped => {}
                RowState::Failed { error, .. } => {
                    failed = true;
                    quota |= error.starts_with("Download limit reached");
                }
                _ => open = true,
            }
        }
        match (any, failed, open) {
            (false, _, _) => None,
            (_, true, _) => Some(RunOutcome::Failed { quota }),
            (_, _, true) => Some(RunOutcome::Queued),
            _ => Some(RunOutcome::Saved),
        }
    }

    /// The run rows of a link: its one file, or (for a folder parent) every ticked child.
    pub fn link_job_rows(&self, link: usize, child: Option<Option<usize>>) -> Vec<&Row> {
        match child {
            Some(c) => self.link_jobs.get(&(link, c)).and_then(|&i| self.rows.get(i)).into_iter().collect(),
            None => self
                .rows
                .iter()
                .filter(|r| matches!(r.origin, JobOrigin::Link { link: l, child: Some(_) } if l == link))
                .collect(),
        }
    }

    /// Seconds left of an external host's cool-down.
    pub fn host_cooldown_secs(&self, host: ExtHost, now: Instant) -> Option<u64> {
        let until = *self.cooldowns.get(&host)?;
        (until > now).then(|| until.saturating_duration_since(now).as_secs_f64().ceil() as u64)
    }

    /// Files that are on disk: downloaded now or already there.
    pub fn saved(&self) -> usize {
        self.count(|s| matches!(s, RowState::Done | RowState::Skipped))
    }

    /// Scanned files the server answered 404 for when sized: not downloadable.
    pub fn missing(&self) -> usize {
        self.files.iter().filter(|f| f.missing).count()
    }

    /// Everything the Problems tab lists: failed downloads plus the files Pawchive no longer has.
    pub fn problem_count(&self) -> usize {
        self.failed() + self.missing()
    }

    pub fn failed(&self) -> usize {
        self.count(|s| matches!(s, RowState::Failed { .. }))
    }

    pub fn retryable(&self) -> usize {
        self.count(|s| matches!(s, RowState::Failed { permanent: false, .. }))
    }

    pub fn active(&self) -> usize {
        self.count(|s| matches!(s, RowState::Active))
    }

    /// Files "Download remaining" would still fetch: everything not saved and not failed.
    pub fn remaining(&self) -> usize {
        self.count(|s| matches!(s, RowState::Queued | RowState::Active | RowState::Retrying { .. } | RowState::Stopped))
    }

    /// Partial `.part` files on disk: started but not finished, not failed.
    pub fn partial(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| {
                r.done > 0 && matches!(r.state, RowState::Active | RowState::Retrying { .. } | RowState::Stopped)
            })
            .count()
    }

    pub fn bytes_done(&self) -> u64 {
        self.rows.iter().map(|r| r.done).sum()
    }

    /// Files that were already on disk and left alone.
    pub fn skipped(&self) -> usize {
        self.count(|s| matches!(s, RowState::Skipped))
    }

    /// Bytes written by this run: skipped files were already there, so they do not count.
    pub fn bytes_downloaded(&self) -> u64 {
        self.rows.iter().filter(|r| matches!(r.state, RowState::Done)).map(|r| r.done).sum()
    }

    pub fn bytes_saved(&self) -> u64 {
        self.rows.iter().filter(|r| matches!(r.state, RowState::Done | RowState::Skipped)).map(|r| r.done).sum()
    }

    /// Total bytes of the run, only when every file's size is known.
    pub fn bytes_total(&self) -> Option<u64> {
        let mut sum = 0;
        for r in &self.rows {
            sum += r.total?;
        }
        (!self.rows.is_empty()).then_some(sum)
    }

    /// Overall progress 0..=1: by bytes when every size is known, otherwise by file count.
    pub fn fraction(&self) -> f32 {
        match self.bytes_total() {
            Some(t) if t > 0 => (self.bytes_done() as f64 / t as f64).clamp(0.0, 1.0) as f32,
            _ if self.rows.is_empty() => 0.0,
            _ => self.saved() as f32 / self.rows.len() as f32,
        }
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.run_elapsed
            .unwrap_or_else(|| self.run_started.map_or(Duration::ZERO, |s| now.saturating_duration_since(s)))
    }

    pub fn scan_elapsed(&self, now: Instant) -> Duration {
        self.scan_started.map_or(Duration::ZERO, |s| now.saturating_duration_since(s))
    }

    pub fn scan_stalled(&self, now: Instant) -> bool {
        self.phase == Phase::Scanning
            && self
                .scan_last_progress
                .or(self.scan_started)
                .is_some_and(|t| now.saturating_duration_since(t) >= SCAN_STALL)
    }

    /// Seconds left of the global cool-down, rounded up; `None` when not slowed down.
    pub fn slow_secs(&self, now: Instant) -> Option<u64> {
        let until = self.slow_until?;
        (until > now).then(|| until.saturating_duration_since(now).as_secs_f64().ceil() as u64)
    }

    /// Bytes per second over the last 5 s; `None` until there is at least a second of data.
    pub fn speed(&self) -> Option<f64> {
        let (t0, b0) = *self.samples.front()?;
        let (t1, b1) = *self.samples.back()?;
        let dt = t1.saturating_duration_since(t0).as_secs_f64();
        (dt >= 1.0).then(|| b1.saturating_sub(b0) as f64 / dt)
    }

    /// Seconds left; only after 15 s of data, with known sizes, and while actually transferring.
    pub fn eta_secs(&self, now: Instant) -> Option<u64> {
        if self.phase != Phase::Downloading || self.slow_secs(now).is_some() || self.elapsed(now) < ETA_AFTER {
            return None;
        }
        let total = self.bytes_total()?;
        let speed = self.speed().filter(|s| *s > 0.0)?;
        Some((total.saturating_sub(self.bytes_done()) as f64 / speed).ceil() as u64)
    }

    // ---- clock ----------------------------------------------------------------------------

    /// Called every frame: ends the cool-down and keeps the speed window.
    pub fn tick(&mut self, now: Instant) {
        self.cooldowns.retain(|_, until| *until > now);
        if self.slow_until.is_some_and(|u| u <= now) {
            self.slow_until = None;
            if self.phase == Phase::Downloading {
                self.push_log(Level::Info, "Cool-down over: requests continue");
            }
        }
        if self.phase != Phase::Downloading || self.slow_until.is_some() {
            self.samples.clear();
            return;
        }
        if self.samples.back().is_none_or(|(t, _)| now.saturating_duration_since(*t) >= Duration::from_millis(500)) {
            let bytes = self.bytes_done();
            self.samples.push_back((now, bytes));
        }
        while self.samples.front().is_some_and(|(t, _)| now.saturating_duration_since(*t) > SPEED_WINDOW) {
            self.samples.pop_front();
        }
    }

    // ---- user actions the UI takes immediately --------------------------------------------

    pub fn note_pause(&mut self) {
        if self.phase == Phase::Downloading {
            self.phase = Phase::Paused;
            self.push_log(Level::Info, pause_line(self.partial()));
        }
    }

    pub fn note_resume(&mut self) {
        if self.phase == Phase::Paused {
            self.phase = Phase::Downloading;
            // The session confirms with its own "Resumed" log event; logging here too printed it twice.
        }
    }

    /// The user pressed Cancel (scan or download); the session confirms with its own event.
    pub fn note_cancel(&mut self) {
        if self.phase.busy() {
            self.cancel_requested = true;
        }
    }

    /// Re-queue failed rows (`ids` are row indexes) for a retry run and show it as downloading.
    pub fn begin_retry(&mut self, ids: &[usize]) {
        for &i in ids {
            if let Some(r) = self.rows.get_mut(i) {
                r.state = RowState::Queued;
                r.done = 0;
            }
        }
        self.phase = Phase::Downloading;
        self.summary = None;
        self.cancel_requested = false;
        self.slow_until = None;
        self.run_started = Some(Instant::now());
        self.run_elapsed = None;
        self.samples.clear();
        self.push_log(Level::Info, format!("Retrying {} files", ids.len()));
    }

    /// True once after a run ends (card flips, tab may switch, window may ask for attention).
    pub fn take_finished(&mut self) -> bool {
        std::mem::take(&mut self.finished_flag)
    }

    // ---- session events -------------------------------------------------------------------

    pub fn apply(&mut self, ev: UiEvent, now: Instant) {
        match ev {
            UiEvent::Log(m) => {
                let level = if ["Could not", "Cannot", "Busy"].iter().any(|p| m.starts_with(p)) {
                    Level::Warn
                } else {
                    Level::Info
                };
                self.push_log(level, m);
            }
            UiEvent::ScanStarted(target) => {
                let c = target.creator.clone();
                self.scan_target = Some(target);
                self.scan_post = None;
                self.links.clear();
                self.links_posts = 0;
                self.link_filter = LinkFilter::All;
                self.checking = None;
                self.awaiting_checks = false;
                self.cooldowns.clear();
                self.link_jobs.clear();
                self.phase = Phase::Scanning;
                self.cancel_requested = false;
                self.scan_failure = None;
                self.files.clear();
                self.files_version += 1;
                self.rows.clear();
                self.summary = None;
                self.posts = 0;
                self.slow_until = None;
                self.scan_progress = ScanProgress::default();
                self.scan_started = Some(now);
                self.scan_last_progress = Some(now);
                self.sizing = Sizing::Finished { skipped: false };
                self.push_log(Level::Info, format!("Scanning {}/{} …", c.service.as_str(), c.id));
                self.creator = Some(c);
            }
            UiEvent::ScanProgress(p) => {
                self.scan_progress = p;
                self.scan_last_progress = Some(now);
            }
            UiEvent::ScanFinished { creator, posts, files, links: found, post } => {
                let n_links = match found.len() {
                    0 => String::new(),
                    1 => ", 1 external link".to_string(),
                    n => format!(", {n} external links"),
                };
                let n_files = if files.len() == 1 { "1 file".to_string() } else { format!("{} files", files.len()) };
                self.push_log(
                    Level::Info,
                    match &post {
                        Some(p) => format!(
                            "Scan finished: post {}, {n_files}{n_links} ({}/{})",
                            p.id,
                            creator.service.as_str(),
                            creator.id
                        ),
                        None => format!(
                            "Scan finished: {posts} posts, {} files{n_links} ({}/{})",
                            files.len(),
                            creator.service.as_str(),
                            creator.id
                        ),
                    },
                );
                self.links_posts = links::posts_with_links(&found);
                self.links = found.into_iter().map(LinkRow::new).collect();
                self.scan_post = post;
                let to_size = files.iter().filter(|f| !f.preview_only).count();
                self.sizing = if to_size > 0 {
                    Sizing::Running { done: 0, total: to_size }
                } else {
                    Sizing::Finished { skipped: false }
                };
                self.creator = Some(creator);
                self.posts = posts;
                self.files = files;
                self.files_version += 1;
                self.phase = Phase::Ready;
            }
            UiEvent::ScanFailed(m) => {
                if self.phase != Phase::Scanning {
                    // Rejected before it started (the link check in the UI normally catches these).
                    self.push_log(Level::Error, format!("Scan failed: {m}"));
                } else if self.cancel_requested {
                    self.cancel_requested = false;
                    self.push_log(Level::Info, "Scan cancelled by you");
                    self.phase = Phase::Empty;
                } else {
                    self.push_log(Level::Error, format!("Scan failed: {m}"));
                    self.scan_failure = Some(ScanFailure { message: m, progress: self.scan_progress });
                    self.posts = self.scan_progress.posts;
                    self.phase = Phase::ScanError;
                }
            }
            UiEvent::SizingProgress { done, total } => self.sizing = Sizing::Running { done, total },
            UiEvent::FileSized { index, size, content_type } => {
                if let Some(f) = self.files.get_mut(index) {
                    f.apply_probe(size, content_type);
                    self.files_version += 1;
                }
            }
            UiEvent::FileMissing { index } => {
                if let Some(f) = self.files.get_mut(index) {
                    f.missing = true;
                    self.files_version += 1;
                }
            }
            UiEvent::SizingFinished { skipped } => {
                self.sizing = Sizing::Finished { skipped };
                let sized = self.files.iter().filter(|f| !f.preview_only && f.size.is_some()).count();
                let all = self.files.iter().filter(|f| !f.preview_only).count();
                let gone = self.missing();
                let bytes: u64 = self.files.iter().filter(|f| !f.preview_only).filter_map(|f| f.size).sum();
                self.push_log(
                    Level::Info,
                    if skipped {
                        format!("Sizing skipped by you: {sized} of {all} files measured")
                    } else {
                        format!("Sizing finished: {sized} of {all} files, {} total", human_size(bytes))
                    },
                );
                if gone > 0 {
                    // One line, not one per file: a creator can have thousands.
                    self.push_log(
                        Level::Warn,
                        format!("{gone} of {all} files are no longer on Pawchive (HTTP 404) and can't be downloaded"),
                    );
                }
            }
            UiEvent::LinkChecking { link } => {
                if let Some(r) = self.links.get_mut(link) {
                    r.check = Check::Checking;
                }
            }
            UiEvent::LinkResolved { link, resolved } => {
                if let Some(r) = self.links.get_mut(link) {
                    r.resolve(resolved);
                }
            }
            UiEvent::LinkDead { link, reason } => {
                if let Some(r) = self.links.get_mut(link) {
                    if let Some(h) = r.ext_host() {
                        let text = reason.text(h);
                        self.push_log(Level::Warn, format!("Dead link ({}): {text}", h.name()));
                    }
                }
                if let Some(r) = self.links.get_mut(link) {
                    r.kill(reason);
                }
            }
            UiEvent::LinkCheckFailed { link, message } => {
                if let Some(r) = self.links.get_mut(link) {
                    r.check = match message {
                        Some(m) => Check::Unavailable(m),
                        None => Check::NotChecked,
                    };
                }
            }
            UiEvent::CheckProgress { done, total } => self.checking = Some((done, total)),
            UiEvent::CheckFinished { skipped } => {
                self.checking = None;
                if skipped {
                    self.push_log(Level::Info, "Link checking skipped by you");
                }
            }
            UiEvent::HostCooldown { host, secs } => self.host_cooldown(host, secs, now),
            UiEvent::DownloadWaiting => {
                self.awaiting_checks = true;
                self.push_log(Level::Info, "Checking the picked links before the download starts");
            }
            UiEvent::DownloadStarted { jobs } => {
                self.awaiting_checks = false;
                self.phase = Phase::Downloading;
                self.summary = None;
                self.cancel_requested = false;
                self.slow_until = None;
                self.run_started = Some(now);
                self.run_elapsed = None;
                self.samples.clear();
                self.checking = None;
                self.link_jobs.clear();
                self.rows = jobs
                    .into_iter()
                    .enumerate()
                    .map(|(i, j)| {
                        // A link file's label is `post folder/path inside the link/leaf`; a Pawchive file's
                        // is `post folder/leaf`.
                        let linked = matches!(j.origin, JobOrigin::Link { .. });
                        let split = if linked { j.label.split_once('/') } else { j.label.rsplit_once('/') };
                        let (post, name) = split.unwrap_or(("", j.label.as_str()));
                        if let JobOrigin::Link { link, child } = j.origin {
                            self.link_jobs.insert((link, child), i);
                        }
                        Row { origin: j.origin, renamed: j.renamed, ..Row::new(post, name, j.expected_size) }
                    })
                    .collect();
                self.push_log(Level::Info, format!("Download started: {} files", self.rows.len()));
            }
            UiEvent::Download(e) => self.apply_download(e, now),
            // The saved library arrives in the next commit.
            UiEvent::Profiles(_)
            | UiEvent::UpdateStarted { .. }
            | UiEvent::UpdateStep(_)
            | UiEvent::UpdateFailed { .. }
            | UiEvent::UpdateCancelled
            | UiEvent::ProfileLoaded(_)
            | UiEvent::ProfileRemoved { .. }
            | UiEvent::FileMarks(_) => {}
            UiEvent::DownloadFinished(s) => {
                if std::mem::take(&mut self.awaiting_checks) {
                    // The wait ended without a download (cancelled, or nothing left to fetch): the phase
                    // (Ready, or Cancelled after an earlier run) and that run's rows stay as they were.
                    return;
                }
                if self.rows.is_empty() {
                    // Nothing was queued; stay on the results.
                    self.phase = Phase::Ready;
                    return;
                }
                self.run_elapsed = self.run_started.map(|t| now.saturating_duration_since(t));
                self.slow_until = None;
                let saved = self.saved();
                let failed = self.failed();
                if self.cancel_requested {
                    self.push_log(
                        Level::Info,
                        format!(
                            "Cancelled by you. {} partial files kept (.part), {} files not started.",
                            self.partial(),
                            self.remaining().saturating_sub(self.partial())
                        ),
                    );
                    self.phase = Phase::Cancelled;
                } else {
                    self.push_log(Level::Info, format!("Finished: {saved} saved, {failed} failed"));
                    self.phase = Phase::Finished;
                }
                self.cancel_requested = false;
                self.summary = Some(s);
                self.finished_flag = true;
            }
        }
    }

    fn apply_download(&mut self, e: Event, now: Instant) {
        match e {
            Event::Started { id } => self.set_state(id, RowState::Active),
            Event::Progress { id, done, total } => {
                if let Some(r) = self.rows.get_mut(id as usize) {
                    r.done = done;
                    r.total = total.or(r.total);
                    if matches!(r.state, RowState::Queued | RowState::Retrying { .. }) {
                        r.state = RowState::Active;
                    }
                }
            }
            Event::Retrying { id, attempt, wait, reason } => {
                let name = self.name(id);
                self.push_log(Level::Warn, format!("Retry {attempt} in {} s: {reason} ({name})", wait.as_secs()));
                self.set_state(id, RowState::Retrying { attempt, until: now + wait });
            }
            Event::GlobalPause { secs } => {
                self.slow_until = Some(now + Duration::from_secs(secs));
                self.samples.clear();
                self.push_log(
                    Level::Warn,
                    format!("The site returned its \"are you a bot\" page (403). Pausing all requests for {secs} s."),
                );
            }
            Event::Skipped { id } => {
                if let Some(r) = self.rows.get_mut(id as usize) {
                    r.done = r.total.unwrap_or(r.done);
                }
                self.set_state(id, RowState::Skipped);
            }
            Event::Done { id, bytes } => {
                if let Some(r) = self.rows.get_mut(id as usize) {
                    r.done = bytes;
                    r.total = Some(bytes);
                }
                self.set_state(id, RowState::Done);
            }
            Event::Failed { id, error, permanent } => {
                let name = self.name(id);
                self.push_log(Level::Error, format!("Failed: {name} — {error}"));
                self.set_state(id, RowState::Failed { error, permanent });
            }
            Event::Cancelled { id } => self.set_state(id, RowState::Stopped),
            Event::Meta { id, name, content_type } => {
                if let Some(r) = self.rows.get_mut(id as usize) {
                    r.real_name = name;
                    r.content_type = content_type;
                }
            }
            Event::ExtCooldown { host, secs } => self.host_cooldown(host, secs, now),
        }
    }

    fn host_cooldown(&mut self, host: ExtHost, secs: u64, now: Instant) {
        if secs == 0 {
            return;
        }
        self.cooldowns.insert(host, now + Duration::from_secs(secs));
        self.push_log(
            Level::Warn,
            format!("{} asked us to slow down. Its requests pause for {secs} s; other sites carry on.", host.name()),
        );
    }

    fn set_state(&mut self, id: u64, state: RowState) {
        if let Some(r) = self.rows.get_mut(id as usize) {
            r.state = state;
        }
    }

    fn name(&self, id: u64) -> String {
        self.rows.get(id as usize).map(|r| r.name.clone()).unwrap_or_default()
    }
}

/// Log line for the moment the user pauses: `"Paused by you."`, `"... 1 partial file kept."`, `"... 3 partial files kept."`.
fn pause_line(partial: usize) -> String {
    match partial {
        0 => "Paused by you.".into(),
        1 => "Paused by you. 1 partial file kept.".into(),
        n => format!("Paused by you. {n} partial files kept."),
    }
}

/// Plain-language reason for a failed file (`ui-spec` section 8, Problems).
pub fn explain_failure(error: &str) -> String {
    let tries =
        error.strip_prefix("gave up after ").and_then(|r| r.split(' ').next()).and_then(|n| n.parse::<u32>().ok());
    let http = error
        .split("HTTP ")
        .nth(1)
        .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|c| c.parse::<u16>().ok());
    match (http, tries) {
        (Some(404), _) => "Not on Pawchive (404). Pawchive no longer has this file.".into(),
        (Some(403 | 429), _) => "The site kept asking us to slow down".into(),
        (Some(code), Some(n)) => format!("Server error ({code}) after {n} tries"),
        (Some(code), None) => format!("Server error ({code})"),
        (None, Some(n)) if error.contains("timed out") => format!("Connection timed out after {n} tries"),
        (None, Some(n)) => format!("Connection failed after {n} tries"),
        (None, None) if error.starts_with("cannot ") || error.starts_with("write failed") => {
            format!("Couldn't write the file: {error}")
        }
        (None, None) => error.to_string(),
    }
}

/// Second line of the scan-error card: the cause, then what was found before it stopped.
pub fn explain_scan_error(message: &str, posts: usize) -> String {
    let found = match posts {
        0 => "No posts were found before it stopped.".to_string(),
        1 => "1 post was found before it stopped.".to_string(),
        n => format!("{} posts were found before it stopped.", crate::format::comma(n)),
    };
    let cause = if let Some(code) = message
        .strip_prefix("server answered HTTP ")
        .and_then(|r| r.split(' ').next())
        .or_else(|| message.split("HTTP ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()))
        .filter(|c| !c.is_empty())
    {
        format!("pawchive.pw answered with an error (HTTP {code}).")
    } else if message.contains("could not parse") || message.contains("repeats the previous page") {
        "Pawchive returned something we couldn't read.".to_string()
    } else if message.contains("timed out") {
        "Couldn't reach pawchive.pw (connection timed out).".to_string()
    } else {
        "Couldn't reach pawchive.pw.".to_string()
    };
    format!("{cause} {found}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pawchive_core::model::Service;
    use pawchive_core::session::JobInfo;

    fn started(n: u64) -> (Model, Instant) {
        let now = Instant::now();
        let mut m = Model::default();
        m.apply(
            UiEvent::DownloadStarted {
                jobs: (0..n)
                    .map(|id| JobInfo {
                        id,
                        label: format!("post{id}/f{id}.jpg"),
                        expected_size: Some(100),
                        origin: JobOrigin::Pawchive,
                        renamed: false,
                    })
                    .collect(),
            },
            now,
        );
        (m, now)
    }

    fn creator() -> CreatorRef {
        CreatorRef { service: Service::Fanbox, id: "1".into() }
    }

    fn target() -> ScanTarget {
        ScanTarget { creator: creator(), post_id: None }
    }

    #[test]
    fn scan_lifecycle_and_error_keeps_what_was_found() {
        let now = Instant::now();
        let mut m = Model::default();
        m.apply(UiEvent::ScanStarted(target()), now);
        assert_eq!(m.phase, Phase::Scanning);
        m.apply(UiEvent::ScanProgress(ScanProgress { pages: 13, posts: 650, files: 3000 }), now);
        m.apply(UiEvent::ScanFailed("request failed: operation timed out".into()), now);
        assert_eq!(m.phase, Phase::ScanError);
        let f = m.scan_failure.as_ref().unwrap();
        assert_eq!((f.progress.pages, m.posts), (13, 650));
        assert_eq!(
            explain_scan_error(&f.message, m.posts),
            "Couldn't reach pawchive.pw (connection timed out). 650 posts were found before it stopped."
        );
        m.apply(UiEvent::ScanStarted(target()), now);
        m.apply(UiEvent::ScanFinished { creator: creator(), posts: 3, files: vec![], links: vec![], post: None }, now);
        assert_eq!(m.phase, Phase::Ready);
    }

    #[test]
    fn cancelling_a_scan_returns_to_empty_without_an_error() {
        let now = Instant::now();
        let mut m = Model::default();
        m.apply(UiEvent::ScanStarted(target()), now);
        m.note_cancel();
        m.apply(UiEvent::ScanFailed("cancelled".into()), now);
        assert_eq!(m.phase, Phase::Empty);
        assert!(m.scan_failure.is_none());
    }

    #[test]
    fn scan_stalls_after_ten_seconds_without_a_page() {
        let t0 = Instant::now();
        let mut m = Model::default();
        m.apply(UiEvent::ScanStarted(target()), t0);
        assert!(!m.scan_stalled(t0 + Duration::from_secs(9)));
        assert!(m.scan_stalled(t0 + Duration::from_secs(10)));
        m.apply(UiEvent::ScanProgress(ScanProgress { pages: 1, posts: 50, files: 5 }), t0 + Duration::from_secs(10));
        assert!(!m.scan_stalled(t0 + Duration::from_secs(12)));
    }

    #[test]
    fn row_states_follow_engine_events_and_counts_agree() {
        let (mut m, now) = started(5);
        m.apply(UiEvent::Download(Event::Started { id: 0 }), now);
        m.apply(UiEvent::Download(Event::Progress { id: 0, done: 50, total: Some(100) }), now);
        assert_eq!((m.rows[0].done, m.rows[0].percent()), (50, Some(0.5)));
        m.apply(UiEvent::Download(Event::Done { id: 0, bytes: 100 }), now);
        m.apply(UiEvent::Download(Event::Skipped { id: 1 }), now);
        m.apply(
            UiEvent::Download(Event::Retrying {
                id: 2,
                attempt: 2,
                wait: Duration::from_secs(4),
                reason: "HTTP 503".into(),
            }),
            now,
        );
        assert!(matches!(m.rows[2].state, RowState::Retrying { attempt: 2, .. }));
        m.apply(UiEvent::Download(Event::Progress { id: 2, done: 1, total: None }), now);
        assert_eq!(m.rows[2].state, RowState::Active, "progress after a retry means it runs again");
        m.apply(UiEvent::Download(Event::Failed { id: 3, error: "HTTP 404: not found".into(), permanent: true }), now);
        m.apply(
            UiEvent::Download(Event::Failed {
                id: 4,
                error: "gave up after 5 tries: HTTP 503".into(),
                permanent: false,
            }),
            now,
        );
        assert_eq!((m.saved(), m.failed(), m.retryable(), m.active()), (2, 2, 1, 1));
        assert_eq!(m.bytes_saved(), 200, "a skipped file counts at its expected size");
        assert!(m.log.iter().any(|l| l.level == Level::Error && l.text.contains("f3.jpg") && l.text.contains("404")));
    }

    #[test]
    fn pause_line_pluralises_and_drops_the_zero_count() {
        assert_eq!(pause_line(0), "Paused by you.");
        assert_eq!(pause_line(1), "Paused by you. 1 partial file kept.");
        assert_eq!(pause_line(2), "Paused by you. 2 partial files kept.");
    }

    #[test]
    fn pause_resume_and_cancel_drive_the_phase() {
        let (mut m, now) = started(3);
        m.apply(UiEvent::Download(Event::Started { id: 0 }), now);
        m.apply(UiEvent::Download(Event::Progress { id: 0, done: 40, total: Some(100) }), now);
        m.note_pause();
        assert_eq!(m.phase, Phase::Paused);
        assert_eq!(m.log.back().unwrap().text, "Paused by you. 1 partial file kept.");
        let log_len = m.log.len();
        m.note_resume();
        assert_eq!(m.phase, Phase::Downloading);
        // The session's own "Resumed" event is the one log line; the model must not add a second.
        assert_eq!(m.log.len(), log_len);
        m.note_cancel();
        m.apply(UiEvent::Download(Event::Cancelled { id: 0 }), now);
        m.apply(
            UiEvent::DownloadFinished(Summary { cancelled: 3, ..Default::default() }),
            now + Duration::from_secs(5),
        );
        assert_eq!(m.phase, Phase::Cancelled);
        assert_eq!((m.remaining(), m.partial()), (3, 1));
        assert_eq!(m.elapsed(now + Duration::from_secs(99)), Duration::from_secs(5), "the clock stops at the end");
        assert!(m.take_finished());
        assert!(!m.take_finished());
    }

    #[test]
    fn finishing_with_failures_and_retrying_them() {
        let (mut m, now) = started(2);
        m.apply(UiEvent::Download(Event::Done { id: 0, bytes: 100 }), now);
        m.apply(
            UiEvent::Download(Event::Failed {
                id: 1,
                error: "gave up after 5 tries: HTTP 503".into(),
                permanent: false,
            }),
            now,
        );
        m.apply(UiEvent::DownloadFinished(Summary { done: 1, failed: 1, ..Default::default() }), now);
        assert_eq!((m.phase, m.failed(), m.retryable()), (Phase::Finished, 1, 1));
        m.begin_retry(&[1]);
        assert_eq!((m.phase, m.failed()), (Phase::Downloading, 0));
        m.apply(UiEvent::Download(Event::Done { id: 1, bytes: 100 }), now);
        m.apply(UiEvent::DownloadFinished(Summary { done: 1, ..Default::default() }), now);
        assert_eq!(
            (m.phase, m.saved(), m.failed()),
            (Phase::Finished, 2, 0),
            "counts come from rows, not the retry summary"
        );
    }

    #[test]
    fn bot_page_cools_down_for_sixty_seconds_then_clears() {
        let (mut m, now) = started(1);
        m.apply(UiEvent::Download(Event::GlobalPause { secs: 60 }), now);
        assert_eq!(m.slow_secs(now), Some(60));
        assert_eq!(m.slow_secs(now + Duration::from_millis(17_500)), Some(43));
        m.tick(now + Duration::from_secs(61));
        assert_eq!(m.slow_secs(now + Duration::from_secs(61)), None);
        assert!(m.slow_until.is_none());
    }

    #[test]
    fn speed_is_a_five_second_average_and_eta_waits_for_fifteen_seconds() {
        let (mut m, t0) = started(1);
        m.rows[0].total = Some(1_000_000);
        for s in 0..=20u64 {
            m.rows[0].done = s * 10_000;
            m.tick(t0 + Duration::from_secs(s));
        }
        let speed = m.speed().unwrap();
        assert!((speed - 10_000.0).abs() < 1.0, "{speed}");
        assert_eq!(m.eta_secs(t0 + Duration::from_secs(10)), None, "under 15 s of data");
        assert_eq!(m.eta_secs(t0 + Duration::from_secs(20)), Some(80));
        m.note_pause();
        assert_eq!(m.eta_secs(t0 + Duration::from_secs(20)), None, "no ETA while paused");
    }

    #[test]
    fn progress_is_by_bytes_when_sizes_are_known_and_by_count_otherwise() {
        let (mut m, now) = started(4);
        m.apply(UiEvent::Download(Event::Done { id: 0, bytes: 100 }), now);
        m.apply(UiEvent::Download(Event::Progress { id: 1, done: 100, total: Some(100) }), now);
        assert_eq!((m.bytes_total(), m.fraction()), (Some(400), 0.5));
        m.rows[2].total = None;
        assert_eq!(m.bytes_total(), None);
        assert_eq!(m.fraction(), 0.25, "1 of 4 files saved");
    }

    #[test]
    fn file_sized_updates_the_list_and_sizing_log_reports_bytes() {
        let now = Instant::now();
        let mut m = Model::default();
        let f = |p: &str| FileEntry {
            post_id: "1".into(),
            post_title: "t".into(),
            post_published: None,
            name: p.into(),
            path: format!("/{p}"),
            preview_only: false,
            node: None,
            size: None,
            content_type: None,
            missing: false,
        };
        m.apply(
            UiEvent::ScanFinished {
                creator: creator(),
                posts: 1,
                files: vec![f("a.jpg"), f("b.jpg")],
                links: vec![],
                post: None,
            },
            now,
        );
        assert_eq!(m.sizing, Sizing::Running { done: 0, total: 2 });
        let v = m.files_version;
        m.apply(UiEvent::FileSized { index: 1, size: Some(10), content_type: Some("image/jpeg".into()) }, now);
        assert_eq!(m.files[1].size, Some(10));
        assert!(m.files_version > v);
        m.apply(UiEvent::FileSized { index: 99, size: None, content_type: None }, now);
        m.apply(UiEvent::SizingFinished { skipped: false }, now);
        assert_eq!(m.log.back().unwrap().text, "Sizing finished: 1 of 2 files, 10 B total");
    }

    #[test]
    fn log_is_capped() {
        let mut m = Model::default();
        for i in 0..(LOG_CAP + 50) {
            m.push_log(Level::Info, format!("{i}"));
        }
        assert_eq!(m.log.len(), LOG_CAP);
        assert_eq!(m.log.front().unwrap().text, "50");
    }

    #[test]
    fn failures_read_in_plain_language() {
        assert_eq!(
            explain_failure("HTTP 404: not found on the server"),
            "Not on Pawchive (404). Pawchive no longer has this file."
        );
        assert_eq!(explain_failure("gave up after 5 tries: HTTP 503"), "Server error (503) after 5 tries");
        assert_eq!(explain_failure("gave up after 5 tries: HTTP 429"), "The site kept asking us to slow down");
        assert_eq!(
            explain_failure("gave up after 5 tries: request failed: operation timed out"),
            "Connection timed out after 5 tries"
        );
        assert_eq!(explain_failure("gave up after 5 tries: connection reset"), "Connection failed after 5 tries");
        assert!(explain_failure("write failed: disk full").starts_with("Couldn't write the file:"));
        assert_eq!(
            explain_scan_error("server answered HTTP 503 for https://x", 700),
            "pawchive.pw answered with an error (HTTP 503). 700 posts were found before it stopped."
        );
        assert!(explain_scan_error("could not parse response: x", 0)
            .starts_with("Pawchive returned something we couldn't read."));
    }

    // ---- external links -----------------------------------------------------------------------------

    fn link_of(id: usize, kind: pawchive_core::links::LinkKind) -> pawchive_core::links::Link {
        pawchive_core::links::Link {
            id,
            url: format!("https://x.test/{id}"),
            host: pawchive_core::links::Host::Mediafire,
            kind,
            name_hint: None,
            posts: vec![PostRef { id: "9".into(), title: "T".into(), published: None }],
        }
    }

    #[test]
    fn a_post_scan_remembers_its_target_and_link_checks_update_the_rows() {
        use pawchive_core::external::{DeadReason, Resolved, ResolvedKind};
        use pawchive_core::links::LinkKind;
        let now = Instant::now();
        let mut m = Model::default();
        let target = ScanTarget { creator: creator(), post_id: Some("9".into()) };
        m.apply(UiEvent::ScanStarted(target), now);
        let kind = LinkKind::MediafireFile { key: "k".into(), page_url: "p".into() };
        let post = PostRef { id: "9".into(), title: "T".into(), published: None };
        m.apply(
            UiEvent::ScanFinished {
                creator: creator(),
                posts: 1,
                files: vec![],
                links: vec![link_of(0, kind.clone()), link_of(1, kind)],
                post: Some(post),
            },
            now,
        );
        assert_eq!((m.links.len(), m.links_posts, m.phase), (2, 1, Phase::Ready));
        assert!(m.scan_post.is_some() && m.scan_target.as_ref().unwrap().post_id.as_deref() == Some("9"));
        assert_eq!(m.log.back().unwrap().text, "Scan finished: post 9, 0 files, 2 external links (fanbox/1)");
        m.apply(UiEvent::LinkChecking { link: 0 }, now);
        assert_eq!(m.links[0].check, Check::Checking);
        m.apply(UiEvent::CheckProgress { done: 0, total: 2 }, now);
        assert_eq!(m.checking, Some((0, 2)));
        let res = Resolved { name: "a.rar".into(), size: Some(5), content_type: None, kind: ResolvedKind::File };
        m.apply(UiEvent::LinkResolved { link: 0, resolved: res }, now);
        m.apply(UiEvent::LinkDead { link: 1, reason: DeadReason::Removed }, now);
        assert!(m.links[0].resolved().is_some() && matches!(m.links[1].check, Check::Dead(DeadReason::Removed)));
        m.apply(UiEvent::LinkCheckFailed { link: 0, message: None }, now);
        assert_eq!(m.links[0].check, Check::NotChecked, "skipped: back to unchecked, no note");
        m.apply(UiEvent::CheckFinished { skipped: false }, now);
        assert_eq!(m.checking, None);
        // A new scan clears the links and the filter.
        m.link_filter = LinkFilter::Drive;
        m.apply(UiEvent::ScanStarted(ScanTarget { creator: creator(), post_id: None }), now);
        assert!(m.links.is_empty() && m.link_filter == LinkFilter::All && m.scan_post.is_none());
    }

    #[test]
    fn link_files_are_rows_of_the_run_but_not_pawchive_rows_and_hosts_cool_down_alone() {
        let now = Instant::now();
        let mut m = Model::default();
        let info = |id: u64, label: &str, origin| pawchive_core::session::JobInfo {
            id,
            label: label.into(),
            expected_size: Some(10),
            origin,
            renamed: false,
        };
        m.apply(
            UiEvent::DownloadStarted {
                jobs: vec![
                    info(0, "post/1.jpg", JobOrigin::Pawchive),
                    info(1, "post/Folder/Lineart/1.jpg", JobOrigin::Link { link: 0, child: Some(2) }),
                    info(2, "post/Super Girl.rar", JobOrigin::Link { link: 1, child: None }),
                ],
            },
            now,
        );
        assert_eq!(m.rows.len(), 3);
        assert_eq!((m.pawchive_rows().count(), m.link_file_rows()), (1, 2), "the Files tab never lists link files");
        assert_eq!((m.rows[1].post.as_str(), m.rows[1].name.as_str()), ("post", "Folder/Lineart/1.jpg"));
        assert_eq!(m.link_jobs[&(0, Some(2))], 1);
        assert_eq!(m.link_jobs[&(1, None)], 2);
        assert_eq!(m.link_job_rows(0, None).len(), 1, "a folder parent sees its children's jobs");
        // What the server said decides the type, never the link text.
        m.apply(
            UiEvent::Download(Event::Meta {
                id: 2,
                name: Some("Super Girl.rar".into()),
                content_type: Some("application/x-rar".into()),
            }),
            now,
        );
        assert_eq!(m.rows[2].content_type.as_deref(), Some("application/x-rar"));
        assert_eq!(m.link_outcome(1), Some(crate::links_state::RunOutcome::Queued));
        m.apply(UiEvent::Download(Event::Done { id: 2, bytes: 10 }), now);
        assert_eq!(m.link_outcome(1), Some(crate::links_state::RunOutcome::Saved));
        m.apply(
            UiEvent::Download(Event::Failed {
                id: 1,
                error: "Download limit reached on Google Drive. Try again later.".into(),
                permanent: false,
            }),
            now,
        );
        assert_eq!(m.link_outcome(0), Some(crate::links_state::RunOutcome::Failed { quota: true }));
        // Only the throttled host cools down; Pawchive's own pause is untouched.
        m.apply(UiEvent::Download(Event::ExtCooldown { host: ExtHost::Drive, secs: 60 }), now);
        assert_eq!(m.host_cooldown_secs(ExtHost::Drive, now), Some(60));
        assert_eq!(m.host_cooldown_secs(ExtHost::Mediafire, now), None);
        assert_eq!(m.slow_secs(now), None, "the global cool-down is Pawchive-only");
        m.tick(now + Duration::from_secs(61));
        assert_eq!(m.host_cooldown_secs(ExtHost::Drive, now + Duration::from_secs(61)), None);
    }
}
