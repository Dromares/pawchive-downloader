//! Pure view-model: everything the screen shows, updated from session events. No egui here, so it
//! is unit-testable.

use std::collections::VecDeque;

use pawchive_core::api::ScanProgress;
use pawchive_core::creator::CreatorRef;
use pawchive_core::engine::{Event, Summary};
use pawchive_core::model::FileEntry;
use pawchive_core::session::UiEvent;

const LOG_CAP: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Scanning,
    Ready,
    Downloading,
    Finished,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    Queued,
    Active,
    Retrying(String),
    Done,
    Skipped,
    Failed { error: String, permanent: bool },
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub label: String,
    pub state: RowState,
    pub done: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sizing {
    Idle,
    Running { done: usize, total: usize },
    Finished { skipped: bool },
}

#[derive(Debug)]
pub struct Model {
    pub phase: Phase,
    pub creator: Option<CreatorRef>,
    pub posts: usize,
    pub scan_progress: ScanProgress,
    pub files: Vec<FileEntry>,
    /// Bumped whenever `files` changes, so cached summaries know when to refresh.
    pub files_version: u64,
    pub sizing: Sizing,
    pub rows: Vec<Row>,
    pub paused: bool,
    pub summary: Option<Summary>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub log: VecDeque<String>,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            phase: Phase::Idle,
            creator: None,
            posts: 0,
            scan_progress: ScanProgress::default(),
            files: Vec::new(),
            files_version: 0,
            sizing: Sizing::Idle,
            rows: Vec::new(),
            paused: false,
            summary: None,
            error: None,
            notice: None,
            log: VecDeque::new(),
        }
    }
}

impl Model {
    pub fn push_log(&mut self, line: impl Into<String>) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line.into());
    }

    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Scanning | Phase::Downloading)
    }

    pub fn failed_rows(&self) -> impl Iterator<Item = (usize, &Row)> {
        self.rows.iter().enumerate().filter(|(_, r)| matches!(r.state, RowState::Failed { .. }))
    }

    pub fn count(&self, pred: impl Fn(&RowState) -> bool) -> usize {
        self.rows.iter().filter(|r| pred(&r.state)).count()
    }

    pub fn finished_rows(&self) -> usize {
        self.count(|s| matches!(s, RowState::Done | RowState::Skipped | RowState::Failed { .. } | RowState::Cancelled))
    }

    pub fn bytes_done(&self) -> u64 {
        self.rows.iter().map(|r| r.done).sum()
    }

    pub fn apply(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Log(m) => self.push_log(m),
            UiEvent::ScanStarted(c) => {
                self.phase = Phase::Scanning;
                self.error = None;
                self.files.clear();
                self.files_version += 1;
                self.rows.clear();
                self.summary = None;
                self.scan_progress = ScanProgress::default();
                self.sizing = Sizing::Idle;
                self.push_log(format!("Scanning {}/{} …", c.service.as_str(), c.id));
                self.creator = Some(c);
            }
            UiEvent::ScanProgress(p) => self.scan_progress = p,
            UiEvent::ScanFinished { creator, posts, files } => {
                self.push_log(format!("Scan finished: {posts} posts, {} files", files.len()));
                self.creator = Some(creator);
                self.posts = posts;
                self.files = files;
                self.files_version += 1;
                self.phase = Phase::Ready;
            }
            UiEvent::ScanFailed(m) => {
                self.push_log(format!("Scan failed: {m}"));
                self.error = Some(m);
                self.phase = if self.files.is_empty() { Phase::Idle } else { Phase::Ready };
            }
            UiEvent::SizingProgress { done, total } => self.sizing = Sizing::Running { done, total },
            UiEvent::FileSized { index, size, content_type } => {
                if let Some(f) = self.files.get_mut(index) {
                    f.apply_probe(size, content_type);
                    self.files_version += 1;
                }
            }
            UiEvent::SizingFinished { skipped } => {
                self.sizing = Sizing::Finished { skipped };
                self.push_log(if skipped { "Size probing stopped" } else { "Size probing finished" });
            }
            UiEvent::DownloadStarted { jobs } => {
                self.phase = Phase::Downloading;
                self.paused = false;
                self.summary = None;
                self.notice = None;
                self.rows = jobs
                    .into_iter()
                    .map(|j| Row { label: j.label, state: RowState::Queued, done: 0, total: j.expected_size })
                    .collect();
                self.push_log(format!("Downloading {} files", self.rows.len()));
            }
            UiEvent::Download(e) => self.apply_download(e),
            UiEvent::DownloadFinished(s) => {
                self.push_log(format!(
                    "Finished: {} downloaded, {} skipped, {} failed, {} cancelled",
                    s.done, s.skipped, s.failed, s.cancelled
                ));
                self.summary = Some(s);
                self.phase = Phase::Finished;
                self.paused = false;
                self.notice = None;
            }
        }
    }

    fn apply_download(&mut self, e: Event) {
        let (id, state) = match e {
            Event::Started { id } => (id, Some(RowState::Active)),
            Event::Progress { id, done, total } => {
                if let Some(r) = self.rows.get_mut(id as usize) {
                    r.done = done;
                    r.total = total.or(r.total);
                    if matches!(r.state, RowState::Queued | RowState::Retrying(_)) {
                        r.state = RowState::Active;
                    }
                }
                return;
            }
            Event::Retrying { id, attempt, wait, reason } => {
                self.push_log(format!("Retry {attempt} in {}s: {reason} ({})", wait.as_secs(), self.label(id)));
                (id, Some(RowState::Retrying(reason)))
            }
            Event::GlobalPause { secs } => {
                self.notice = Some(format!("The file host showed a bot page. All requests are paused for {secs} s."));
                self.push_log(format!("Bot page (HTTP 403): pausing all requests for {secs} s"));
                return;
            }
            Event::Skipped { id } => (id, Some(RowState::Skipped)),
            Event::Done { id, bytes } => {
                if let Some(r) = self.rows.get_mut(id as usize) {
                    r.done = bytes;
                    r.total = Some(bytes);
                }
                (id, Some(RowState::Done))
            }
            Event::Failed { id, error, permanent } => {
                self.push_log(format!("Failed: {} — {error}", self.label(id)));
                (id, Some(RowState::Failed { error, permanent }))
            }
            Event::Cancelled { id } => (id, Some(RowState::Cancelled)),
        };
        if let (Some(r), Some(state)) = (self.rows.get_mut(id as usize), state) {
            r.state = state;
        }
    }

    fn label(&self, id: u64) -> String {
        self.rows.get(id as usize).map(|r| r.label.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pawchive_core::model::Service;
    use pawchive_core::session::JobInfo;
    use std::time::Duration;

    fn started(n: u64) -> Model {
        let mut m = Model::default();
        m.apply(UiEvent::DownloadStarted {
            jobs: (0..n).map(|id| JobInfo { id, label: format!("f{id}"), expected_size: None }).collect(),
        });
        m
    }

    #[test]
    fn scan_lifecycle() {
        let mut m = Model::default();
        let c = CreatorRef { service: Service::Fanbox, id: "1".into() };
        m.apply(UiEvent::ScanStarted(c.clone()));
        assert_eq!(m.phase, Phase::Scanning);
        assert!(m.busy());
        m.apply(UiEvent::ScanFinished { creator: c, posts: 3, files: vec![] });
        assert_eq!(m.phase, Phase::Ready);
        m.apply(UiEvent::ScanStarted(CreatorRef { service: Service::Fanbox, id: "2".into() }));
        m.apply(UiEvent::ScanFailed("nope".into()));
        assert_eq!((m.phase, m.error.as_deref()), (Phase::Idle, Some("nope")));
    }

    #[test]
    fn row_states_follow_engine_events() {
        let mut m = started(4);
        m.apply(UiEvent::Download(Event::Started { id: 0 }));
        m.apply(UiEvent::Download(Event::Progress { id: 0, done: 50, total: Some(100) }));
        assert_eq!((m.rows[0].done, m.rows[0].total), (50, Some(100)));
        m.apply(UiEvent::Download(Event::Done { id: 0, bytes: 100 }));
        m.apply(UiEvent::Download(Event::Skipped { id: 1 }));
        m.apply(UiEvent::Download(Event::Retrying {
            id: 2,
            attempt: 1,
            wait: Duration::from_secs(2),
            reason: "HTTP 429".into(),
        }));
        assert!(matches!(m.rows[2].state, RowState::Retrying(_)));
        m.apply(UiEvent::Download(Event::Progress { id: 2, done: 1, total: None }));
        assert_eq!(m.rows[2].state, RowState::Active, "progress after a retry means it is running again");
        m.apply(UiEvent::Download(Event::Failed { id: 3, error: "HTTP 404".into(), permanent: true }));
        assert_eq!(m.failed_rows().map(|(i, _)| i).collect::<Vec<_>>(), [3]);
        assert_eq!(m.finished_rows(), 3);
        assert!(m.log.iter().any(|l| l.contains("Failed: f3") && l.contains("HTTP 404")));
    }

    #[test]
    fn retry_failed_rows_flip_back_when_they_succeed() {
        let mut m = started(1);
        m.apply(UiEvent::Download(Event::Failed { id: 0, error: "x".into(), permanent: false }));
        m.apply(UiEvent::DownloadFinished(Summary { failed: 1, ..Default::default() }));
        assert_eq!(m.phase, Phase::Finished);
        m.apply(UiEvent::Download(Event::Started { id: 0 }));
        m.apply(UiEvent::Download(Event::Done { id: 0, bytes: 9 }));
        assert_eq!(m.failed_rows().count(), 0);
    }

    #[test]
    fn global_pause_sets_a_notice_that_finishing_clears() {
        let mut m = started(1);
        m.apply(UiEvent::Download(Event::GlobalPause { secs: 60 }));
        assert!(m.notice.as_deref().unwrap().contains("60"));
        m.apply(UiEvent::DownloadFinished(Summary::default()));
        assert!(m.notice.is_none());
    }

    #[test]
    fn file_sized_updates_the_list_and_bumps_the_version() {
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
        };
        m.apply(UiEvent::ScanFinished {
            creator: CreatorRef { service: Service::Fanbox, id: "1".into() },
            posts: 1,
            files: vec![f("a.jpg"), f("b.jpg")],
        });
        let v = m.files_version;
        m.apply(UiEvent::FileSized { index: 1, size: Some(10), content_type: Some("image/jpeg".into()) });
        assert_eq!(m.files[1].size, Some(10));
        assert!(m.files_version > v);
        m.apply(UiEvent::FileSized { index: 99, size: None, content_type: None });
        // out of range: ignored
    }

    #[test]
    fn log_is_capped() {
        let mut m = Model::default();
        for i in 0..(LOG_CAP + 50) {
            m.push_log(format!("{i}"));
        }
        assert_eq!(m.log.len(), LOG_CAP);
        assert_eq!(m.log.front().unwrap(), "50");
    }
}
