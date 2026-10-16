//! End-to-end through the command/event layer a GUI uses: scan -> size probing -> download,
//! against the in-process mock server.

mod common;

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use common::{serve, Resp};
use pawchive_core::api::Hosts;
use pawchive_core::classify::Category;
use pawchive_core::http::RetryPolicy;
use pawchive_core::plan::Selection;
use pawchive_core::session::{spawn, Command, Tuning, UiEvent};
use pawchive_core::settings::Settings;
use tempfile::TempDir;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)).unwrap()
}

/// Body for a file path: deterministic and different per path.
fn file_body(path: &str) -> Vec<u8> {
    let seed = path.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    (0..(500 + seed % 700)).map(|i| (i ^ seed) as u8).collect()
}

fn wait_for<T>(rx: &Receiver<UiEvent>, mut pick: impl FnMut(UiEvent) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(ev) => {
                if let Some(v) = pick(ev) {
                    return v;
                }
            }
            Err(e) => panic!("timed out waiting for an event: {e}"),
        }
    }
}

async fn mock_site() -> common::Mock {
    let page = fixture("fanbox-user-119251-posts.json");
    serve(move |req, _| {
        if req.path.starts_with("/api/v1/fanbox/user/119251?o=0") {
            return Resp::ok(&page);
        }
        if req.path.starts_with("/api/") {
            return Resp::ok(b"[]");
        }
        let body = file_body(&req.path);
        let ct = if req.path.ends_with(".png") { "image/png" } else { "image/jpeg" };
        match req.range() {
            Some("bytes=0-0") => Resp::status(206)
                .header("Content-Range", &format!("bytes 0-0/{}", body.len()))
                .header("Content-Type", ct)
                .tap_body(vec![body[0]]),
            _ => Resp::ok(&body).header("Content-Type", ct),
        }
    })
    .await
}

trait TapBody {
    fn tap_body(self, b: Vec<u8>) -> Self;
}
impl TapBody for Resp {
    fn tap_body(mut self, b: Vec<u8>) -> Self {
        self.body = b;
        self
    }
}

fn start(base: &str) -> (pawchive_core::session::SessionHandle, Receiver<UiEvent>) {
    let hosts = Hosts { api: base.into(), file: base.into(), thumb: base.into() };
    let tuning = Tuning {
        retry: RetryPolicy { base: Duration::from_millis(10), ..Default::default() },
        delay_override: Some(Duration::from_millis(1)),
    };
    spawn(Settings::default(), hosts, tuning, Arc::new(|| {}))
}

#[test]
fn scan_size_and_download_end_to_end() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(mock_site());
    let (handle, rx) = start(&mock.base);

    handle.send(Command::Scan { url: "https://pawchive.pw/fanbox/user/119251".into() });
    let files = wait_for(&rx, |e| match e {
        UiEvent::ScanFinished { files, posts, .. } => {
            assert_eq!(posts, 3);
            Some(files)
        }
        UiEvent::ScanFailed(m) => panic!("scan failed: {m}"),
        _ => None,
    });
    assert!(files.len() > 3);

    let mut sized = 0;
    wait_for(&rx, |e| match e {
        UiEvent::FileSized { size, content_type, .. } => {
            assert!(size.is_some() && content_type.is_some());
            sized += 1;
            None
        }
        UiEvent::SizingFinished { skipped } => {
            assert!(!skipped);
            Some(())
        }
        _ => None,
    });
    assert_eq!(sized, files.len(), "one probe per downloadable file");

    let out = TempDir::new().unwrap();
    handle.send(Command::StartDownload {
        selection: Selection { categories: Category::ALL.into_iter().collect(), include_preview: false },
        custom_extensions: String::new(),
        output: out.path().to_path_buf(),
    });
    let summary = wait_for(&rx, |e| match e {
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!((summary.done, summary.failed, summary.skipped), (files.len(), 0, 0), "{summary:?}");

    // every downloaded file has the bytes the server holds for its path
    let mut found: Vec<PathBuf> = vec![];
    for e in walk(out.path()) {
        found.push(e);
    }
    assert_eq!(found.len(), files.len());
    assert!(found.iter().all(|p| !p.to_string_lossy().ends_with(".part")));
    for f in &files {
        let url_path = format!("/data{}", f.path);
        let want = file_body(&url_path);
        assert!(found.iter().any(|p| std::fs::read(p).unwrap() == want), "no downloaded file matches {}", f.name);
    }

    // second run: everything exists with the probed size -> all skipped, no new file requests
    let before = mock.count();
    handle.send(Command::StartDownload {
        selection: Selection { categories: Category::ALL.into_iter().collect(), include_preview: false },
        custom_extensions: String::new(),
        output: out.path().to_path_buf(),
    });
    let summary = wait_for(&rx, |e| match e {
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!((summary.skipped, summary.done), (files.len(), 0), "{summary:?}");
    assert_eq!(mock.count(), before);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            v.extend(walk(&p));
        } else {
            v.push(p);
        }
    }
    v
}

#[test]
fn bad_urls_are_reported_without_touching_the_network() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(mock_site());
    let (handle, rx) = start(&mock.base);
    handle.send(Command::Scan { url: "https://example.com/fanbox/user/1".into() });
    let msg = wait_for(&rx, |e| match e {
        UiEvent::ScanFailed(m) => Some(m),
        _ => None,
    });
    assert!(msg.contains("not Pawchive"), "{msg}");
    assert_eq!(mock.count(), 0);
}

#[test]
fn skip_sizing_stops_the_probes_and_downloads_still_work() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(mock_site());
    let (handle, rx) = {
        let hosts = Hosts { api: mock.base.clone(), file: mock.base.clone(), thumb: mock.base.clone() };
        // slow gate so sizing is still running when we skip it
        let tuning = Tuning { retry: RetryPolicy::default(), delay_override: Some(Duration::from_millis(250)) };
        spawn(Settings::default(), hosts, tuning, Arc::new(|| {}))
    };
    handle.send(Command::Scan { url: "https://pawchive.pw/fanbox/user/119251".into() });
    let n = wait_for(&rx, |e| match e {
        UiEvent::ScanFinished { files, .. } => Some(files.len()),
        _ => None,
    });
    handle.send(Command::SkipSizing);
    let skipped = wait_for(&rx, |e| match e {
        UiEvent::SizingFinished { skipped } => Some(skipped),
        _ => None,
    });
    assert!(skipped);
    let out = TempDir::new().unwrap();
    handle.send(Command::StartDownload {
        selection: Selection { categories: Category::ALL.into_iter().collect(), include_preview: false },
        custom_extensions: String::new(),
        output: out.path().to_path_buf(),
    });
    let s = wait_for(&rx, |e| match e {
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!(s.done, n);
}
