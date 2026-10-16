//! The download engine: bounded concurrency, global request pacing, retry/backoff, `.part` files
//! with `Range` resume, skip-existing, pause and cancel.

use std::collections::VecDeque;
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

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

impl Engine {
    pub fn new(http: Http, gate: Gate, cfg: EngineConfig) -> Self {
        let cfg = EngineConfig { concurrency: cfg.concurrency.clamp(1, MAX_CONCURRENCY), ..cfg };
        Engine { http, gate, cfg: Arc::new(cfg) }
    }

    /// Run `jobs` to completion (or cancellation). Events are delivered through `emit`.
    pub async fn run(&self, jobs: Vec<Job>, control: Control, emit: impl Fn(Event) + Send + Sync + 'static) -> Summary {
        let total = jobs.len();
        let emit: Emit = Arc::new(emit);
        let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
        let tally = Arc::new(Mutex::new(Summary::default()));
        let mut workers = JoinSet::new();
        for _ in 0..self.cfg.concurrency {
            let (eng, queue, tally, control, emit) =
                (self.clone(), queue.clone(), tally.clone(), control.clone(), emit.clone());
            workers.spawn(async move {
                loop {
                    // Pause stops new starts; cancel stops everything.
                    if !control.wait_while_paused().await {
                        break;
                    }
                    let Some(job) = queue.lock().unwrap().pop_front() else { break };
                    let outcome = eng.transfer(&job, &control, &emit).await;
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
            if !control.wait_while_paused().await || !self.gate.acquire(control).await {
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
        let mut req = self.http.get(&job.url);
        if existing > 0 {
            req = req.header(RANGE, format!("bytes={existing}-"));
        }
        let mut resp = tokio::select! {
            r = req.send() => match r {
                Ok(r) => r,
                Err(e) => return Step::Retry { reason: Error::from_reqwest(e).to_string(), retry_after: None, bot_block: false },
            },
            _ = control.cancelled() => return Step::Cancelled,
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
                    return finish(part, &job.dest, existing).await;
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
        finish(part, &job.dest, done).await
    }
}

async fn finish(part: &Path, dest: &Path, bytes: u64) -> Step {
    if fs::metadata(dest).await.is_ok() {
        let _ = fs::remove_file(dest).await; // replacing a stale/mismatched file
    }
    match fs::rename(part, dest).await {
        Ok(()) => Step::Done(bytes),
        Err(e) => Step::Fail { error: format!("cannot move {} into place: {e}", part.display()), permanent: true },
    }
}
