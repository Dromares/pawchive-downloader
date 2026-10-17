//! The download engine: bounded concurrency, global request pacing, retry/backoff, `.part` files
//! with `Range` resume, skip-existing, pause and cancel.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{CONTENT_RANGE, RANGE};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::control::{Control, RunState};
use crate::error::Error;
use crate::external::{self, drive, mediafire, ExtClient, ExtHost, Notice, ResolveError};
use crate::gate::Gate;
use crate::http::{retry_after, Http, RetryPolicy};
use crate::probe::total_from_content_range;
use crate::settings::MAX_CONCURRENCY;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: u64,
    pub url: String,
    /// Final path; data streams to `<dest>.part` first.
    pub dest: PathBuf,
    /// From the size probe, when we have one.
    pub expected_size: Option<u64>,
    /// Shown in the UI: `post folder/filename`.
    pub label: String,
    /// Set for a file on Google Drive or Mediafire: `url` is then unused, and the download link is
    /// found when the job runs.
    pub ext: Option<ExtSource>,
}

/// Where an external file really comes from. Resolved at download time, never stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtSource {
    Mediafire { page_url: String },
    Drive { file_id: String },
}

impl ExtSource {
    pub fn host(&self) -> ExtHost {
        match self {
            ExtSource::Mediafire { .. } => ExtHost::Mediafire,
            ExtSource::Drive { .. } => ExtHost::Drive,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Started {
        id: u64,
    },
    Progress {
        id: u64,
        done: u64,
        total: Option<u64>,
    },
    Retrying {
        id: u64,
        attempt: u32,
        wait: Duration,
        reason: String,
    },
    /// A 403 bot page: every request (all jobs) is paused for this long.
    GlobalPause {
        secs: u64,
    },
    Skipped {
        id: u64,
    },
    Done {
        id: u64,
        bytes: u64,
    },
    Failed {
        id: u64,
        error: String,
        permanent: bool,
    },
    Cancelled {
        id: u64,
    },
    /// What the server said about an external file: its real name (`Content-Disposition`) and
    /// Content-Type. The file's type is decided from these, never from the link text.
    Meta {
        id: u64,
        name: Option<String>,
        content_type: Option<String>,
    },
    /// An external host answered 403/429: only its own requests stop for this long.
    ExtCooldown {
        host: ExtHost,
        secs: u64,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub done: usize,
    pub skipped: usize,
    pub failed: usize,
    /// In flight when cancelled, plus jobs that never started.
    pub cancelled: usize,
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub concurrency: usize,
    pub retry: RetryPolicy,
}

impl EngineConfig {
    pub fn new(concurrency: usize) -> Self {
        EngineConfig { concurrency: concurrency.clamp(1, MAX_CONCURRENCY), retry: RetryPolicy::default() }
    }
}

#[derive(Clone)]
pub struct Engine {
    http: Http,
    gate: Gate,
    cfg: Arc<EngineConfig>,
    ext: Option<Arc<ExtClient>>,
    /// Never replace a file at the destination (the library's rule). Off by default: a download
    /// outside the library still replaces a stale file of the wrong size.
    no_replace: bool,
}

type Emit = Arc<dyn Fn(Event) + Send + Sync>;

enum Outcome {
    Done(u64),
    Skipped,
    Failed { error: String, permanent: bool },
    Cancelled,
}

/// Result of one HTTP attempt.
enum Step {
    Done(u64),
    Fail {
        error: String,
        permanent: bool,
    },
    Cancelled,
    /// Paused mid-transfer; the connection was dropped and the `.part` file kept.
    Paused,
    Retry {
        reason: String,
        retry_after: Option<Duration>,
        bot_block: bool,
    },
}

/// The jobs still to start, plus the external hosts that already have a download running (at most
/// one at a time per external host, so a worker never sits idle behind a busy host).
struct Queue {
    jobs: VecDeque<Job>,
    busy: HashSet<ExtHost>,
}

/// The first queued job whose host is free, marked busy. Waits while every queued job is behind a
/// busy host; `None` when the queue is empty or the run was cancelled.
async fn next_job(queue: &Mutex<Queue>, wake: &tokio::sync::Notify, control: &Control) -> Option<Job> {
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let mut q = queue.lock().unwrap();
            if q.jobs.is_empty() {
                return None;
            }
            let pos = q.jobs.iter().position(|j| j.ext.as_ref().is_none_or(|e| !q.busy.contains(&e.host())));
            if let Some(job) = pos.and_then(|p| q.jobs.remove(p)) {
                if let Some(e) = &job.ext {
                    q.busy.insert(e.host());
                }
                return Some(job);
            }
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = control.cancelled() => return None,
        }
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

impl Engine {
    pub fn new(http: Http, gate: Gate, cfg: EngineConfig) -> Self {
        let cfg = EngineConfig { concurrency: cfg.concurrency.clamp(1, MAX_CONCURRENCY), ..cfg };
        Engine { http, gate, cfg: Arc::new(cfg), ext: None, no_replace: false }
    }

    /// Allow jobs with an [`ExtSource`]. They use this client (no cookies, plain user agent) and the
    /// per-host clocks inside it, never the Pawchive gate.
    pub fn with_external(mut self, ext: ExtClient) -> Self {
        self.ext = Some(Arc::new(ext));
        self
    }

    /// Never overwrite or delete a file at a job's destination. A job whose destination exists is
    /// skipped, and a file that appears there while the transfer runs makes the job fail with the
    /// finished download left in its `.part` file. The final step is a hard link, which cannot
    /// replace; a filesystem without hard links falls back to a check followed by a rename.
    pub fn with_no_replace(mut self) -> Self {
        self.no_replace = true;
        self
    }

    /// Run `jobs` to completion (or cancellation). Events are delivered through `emit`.
    pub async fn run(&self, jobs: Vec<Job>, control: Control, emit: impl Fn(Event) + Send + Sync + 'static) -> Summary {
        let total = jobs.len();
        let emit: Emit = Arc::new(emit);
        let queue = Arc::new(Mutex::new(Queue { jobs: VecDeque::from(jobs), busy: HashSet::new() }));
        let wake = Arc::new(tokio::sync::Notify::new());
        let tally = Arc::new(Mutex::new(Summary::default()));
        let mut workers = JoinSet::new();
        for _ in 0..self.cfg.concurrency {
            let (eng, queue, tally, control, emit, wake) =
                (self.clone(), queue.clone(), tally.clone(), control.clone(), emit.clone(), wake.clone());
            workers.spawn(async move {
                loop {
                    // Pause stops new starts; cancel stops everything.
                    if !control.wait_while_paused().await {
                        break;
                    }
                    let Some(job) = next_job(&queue, &wake, &control).await else { break };
                    let host = job.ext.as_ref().map(ExtSource::host);
                    let outcome = eng.transfer(&job, &control, &emit).await;
                    if let Some(h) = host {
                        queue.lock().unwrap().busy.remove(&h);
                        wake.notify_waiters();
                    }
                    let mut t = tally.lock().unwrap();
                    match outcome {
                        Outcome::Done(bytes) => {
                            t.done += 1;
                            emit(Event::Done { id: job.id, bytes });
                        }
                        Outcome::Skipped => {
                            t.skipped += 1;
                            emit(Event::Skipped { id: job.id });
                        }
                        Outcome::Failed { error, permanent } => {
                            t.failed += 1;
                            emit(Event::Failed { id: job.id, error, permanent });
                        }
                        Outcome::Cancelled => {
                            t.cancelled += 1;
                            emit(Event::Cancelled { id: job.id });
                        }
                    }
                }
            });
        }
        while workers.join_next().await.is_some() {}
        let mut s = tally.lock().unwrap().clone();
        s.cancelled += total - (s.done + s.skipped + s.failed + s.cancelled);
        s
    }

    async fn transfer(&self, job: &Job, control: &Control, emit: &Emit) -> Outcome {
        if let Some(dir) = job.dest.parent() {
            if let Err(e) = fs::create_dir_all(dir).await {
                return Outcome::Failed { error: format!("cannot create {}: {e}", dir.display()), permanent: true };
            }
        }
        if self.no_replace && fs::symlink_metadata(&job.dest).await.is_ok() {
            return Outcome::Skipped;
        }
        if let Ok(md) = fs::metadata(&job.dest).await {
            // Present with the expected size (or, with no expectation, non-empty): nothing to do.
            if md.is_file() && md.len() > 0 && job.expected_size.is_none_or(|s| s == md.len()) {
                return Outcome::Skipped;
            }
        }
        let part = part_path(&job.dest);
        emit(Event::Started { id: job.id });
        let policy = &self.cfg.retry;
        let mut attempt = 0u32;
        loop {
            // External hosts keep their own clocks (inside `ExtClient::get`); the Pawchive gate is not theirs.
            if !control.wait_while_paused().await || (job.ext.is_none() && !self.gate.acquire(control).await) {
                return Outcome::Cancelled;
            }
            match self.attempt(job, &part, control, emit).await {
                Step::Done(bytes) => return Outcome::Done(bytes),
                Step::Fail { error, permanent } => return Outcome::Failed { error, permanent },
                Step::Cancelled => return Outcome::Cancelled,
                Step::Paused => continue, // not a failure: park at the top of the loop, then resume
                Step::Retry { reason, retry_after, bot_block } => {
                    attempt += 1;
                    if attempt >= policy.max_tries {
                        return Outcome::Failed {
                            error: format!("gave up after {attempt} tries: {reason}"),
                            permanent: false,
                        };
                    }
                    // After a bot page the global gate is already paused; no extra per-file wait.
                    let wait = if bot_block { Duration::ZERO } else { policy.backoff(attempt, retry_after) };
                    emit(Event::Retrying { id: job.id, attempt, wait, reason });
                    if !control.sleep(wait).await {
                        return Outcome::Cancelled;
                    }
                }
            }
        }
    }

    async fn attempt(&self, job: &Job, part: &Path, control: &Control, emit: &Emit) -> Step {
        let existing = fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
        let range = (existing > 0).then(|| format!("bytes={existing}-"));
        let mut resp = if let Some(src) = &job.ext {
            match self.open_external(job, src, range, control, emit).await {
                Ok(r) => r,
                Err(step) => return step,
            }
        } else {
            let mut req = self.http.get(&job.url);
            if let Some(r) = range {
                req = req.header(RANGE, r);
            }
            tokio::select! {
                r = req.send() => match r {
                    Ok(r) => r,
                    Err(e) => return Step::Retry { reason: Error::from_reqwest(e).to_string(), retry_after: None, bot_block: false },
                },
                _ = control.cancelled() => return Step::Cancelled,
            }
        };
        let status = resp.status().as_u16();
        let retry = |reason: String, ra| Step::Retry { reason, retry_after: ra, bot_block: false };
        let (start, total) = match status {
            404 => return Step::Fail { error: "HTTP 404: not found on the server".into(), permanent: true },
            403 => {
                let pause = self.cfg.retry.forbidden_pause;
                self.gate.pause_for(pause);
                emit(Event::GlobalPause { secs: pause.as_secs() });
                return Step::Retry { reason: "HTTP 403 (bot page)".into(), retry_after: None, bot_block: true };
            }
            429 | 500..=599 => return retry(format!("HTTP {status}"), retry_after(resp.headers())),
            416 if existing > 0 => {
                // Our .part is at or beyond the end: complete if it matches, otherwise start over.
                if job.expected_size == Some(existing) {
                    return finish(part, &job.dest, existing, self.no_replace).await;
                }
                let _ = fs::remove_file(part).await;
                return retry("range not satisfiable; restarting".into(), None);
            }
            200 => (0, resp.content_length()),
            206 => {
                let range = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).unwrap_or("");
                let start: Option<u64> =
                    range.strip_prefix("bytes ").and_then(|r| r.split('-').next()).and_then(|s| s.parse().ok());
                if start != Some(existing) {
                    let _ = fs::remove_file(part).await;
                    return retry("server returned a different range; restarting".into(), None);
                }
                (existing, total_from_content_range(range))
            }
            other => return Step::Fail { error: format!("HTTP {other}"), permanent: true },
        };
        // 200 means the whole body: a server that ignored our Range restarts the file cleanly.
        let file =
            if start > 0 { fs::OpenOptions::new().append(true).open(part).await } else { fs::File::create(part).await };
        let mut file = match file {
            Ok(f) => f,
            Err(e) => return Step::Fail { error: format!("cannot write {}: {e}", part.display()), permanent: true },
        };
        let mut done = start;
        let mut last = Instant::now();
        emit(Event::Progress { id: job.id, done, total });
        loop {
            let chunk = tokio::select! {
                c = resp.chunk() => c,
                _ = control.interrupted() => {
                    let _ = file.flush().await;
                    return match control.state() {
                        RunState::Cancelled => Step::Cancelled,
                        _ => {
                            emit(Event::Progress { id: job.id, done, total });
                            Step::Paused
                        }
                    };
                }
            };
            match chunk {
                Ok(Some(bytes)) => {
                    if let Err(e) = file.write_all(&bytes).await {
                        return Step::Fail { error: format!("write failed: {e}"), permanent: true };
                    }
                    done += bytes.len() as u64;
                    if last.elapsed() >= Duration::from_millis(100) {
                        last = Instant::now();
                        emit(Event::Progress { id: job.id, done, total });
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = file.flush().await;
                    return retry(Error::from_reqwest(e).to_string(), None);
                }
            }
        }
        if let Err(e) = file.flush().await {
            return Step::Fail { error: format!("write failed: {e}"), permanent: true };
        }
        drop(file);
        if let Some(t) = total {
            if done != t {
                return retry(format!("connection closed early ({done} of {t} bytes)"), None);
            }
        }
        emit(Event::Progress { id: job.id, done, total: Some(done) });
        finish(part, &job.dest, done, self.no_replace).await
    }
}

impl Engine {
    /// Find the real download link of an external file and open it. For Mediafire the file page is
    /// read now and its signed link used at once; for Drive the virus-scan page, if any, is passed.
    async fn open_external(
        &self,
        job: &Job,
        src: &ExtSource,
        range: Option<String>,
        control: &Control,
        emit: &Emit,
    ) -> Result<reqwest::Response, Step> {
        let Some(ext) = &self.ext else {
            return Err(Step::Fail { error: "external downloads are not set up".into(), permanent: true });
        };
        let notify = |n: Notice| match n {
            Notice::Cooldown { host, secs } => emit(Event::ExtCooldown { host, secs }),
        };
        let host = src.host();
        let opened = match src {
            ExtSource::Drive { file_id } => drive::open_file(ext, file_id, range, control, &notify).await,
            ExtSource::Mediafire { page_url } => match mediafire::direct_url(ext, page_url, control, &notify).await {
                Ok((direct, _)) => {
                    let extra: Vec<(&str, String)> = range.map(|r| (RANGE.as_str(), r)).into_iter().collect();
                    match ext.get(host, control, &notify, &direct, &extra).await {
                        Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                            Err(ResolveError::Dead(external::DeadReason::Removed))
                        }
                        Ok(r) if external::is_html(&r) => {
                            Err(ResolveError::transient("Mediafire sent a web page instead of the file"))
                        }
                        Ok(r) => Ok(r),
                        Err(e) => Err(e.into()),
                    }
                }
                Err(e) => Err(e),
            },
        };
        match opened {
            Ok(resp) => {
                let meta = external::download_meta(&resp);
                emit(Event::Meta { id: job.id, name: meta.name, content_type: meta.content_type });
                Ok(resp)
            }
            Err(ResolveError::Cancelled) => Err(Step::Cancelled),
            Err(ResolveError::Dead(r)) => Err(Step::Fail { error: r.text(host), permanent: true }),
            Err(ResolveError::Transient { quota: true, .. }) => Err(Step::Fail {
                error: format!("Download limit reached on {}. Try again later.", host.name()),
                permanent: false,
            }),
            Err(ResolveError::Transient { message, .. }) => {
                Err(Step::Retry { reason: message, retry_after: None, bot_block: false })
            }
        }
    }
}

async fn finish(part: &Path, dest: &Path, bytes: u64, no_replace: bool) -> Step {
    if no_replace {
        return finish_without_replacing(part, dest, bytes).await;
    }
    if fs::metadata(dest).await.is_ok() {
        let _ = fs::remove_file(dest).await; // replacing a stale/mismatched file
    }
    match fs::rename(part, dest).await {
        Ok(()) => Step::Done(bytes),
        Err(e) => Step::Fail { error: format!("cannot move {} into place: {e}", part.display()), permanent: true },
    }
}

/// Move `part` to `dest` only if nothing is there. `hard_link` fails with `AlreadyExists` instead
/// of replacing; the `.part` name is removed once the link is in place.
async fn finish_without_replacing(part: &Path, dest: &Path, bytes: u64) -> Step {
    let exists = |what: &str| Step::Fail {
        error: format!(
            "{what} appeared while downloading; not replacing it (the new copy stays in {})",
            part.display()
        ),
        permanent: true,
    };
    match fs::hard_link(part, dest).await {
        Ok(()) => {
            let _ = fs::remove_file(part).await;
            Step::Done(bytes)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => exists(&dest.display().to_string()),
        Err(_) => {
            // No hard links here (FAT, some network mounts): check, then rename. Not atomic.
            if fs::symlink_metadata(dest).await.is_ok() {
                return exists(&dest.display().to_string());
            }
            match fs::rename(part, dest).await {
                Ok(()) => Step::Done(bytes),
                Err(e) => {
                    Step::Fail { error: format!("cannot move {} into place: {e}", part.display()), permanent: true }
                }
            }
        }
    }
}
