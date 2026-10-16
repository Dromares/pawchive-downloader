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

/// Mock site where chosen file paths answer with a fixed status until `healed` is set.
fn flaky_site(
    rt: &tokio::runtime::Runtime,
    rules: Arc<std::sync::Mutex<std::collections::HashMap<String, u16>>>,
    healed: Arc<std::sync::atomic::AtomicBool>,
) -> common::Mock {
    let page = fixture("fanbox-user-119251-posts.json");
    rt.block_on(serve(move |req, _| {
        if req.path.starts_with("/api/v1/fanbox/user/119251?o=0") {
            return Resp::ok(&page);
        }
        if req.path.starts_with("/api/") {
            return Resp::ok(b"[]");
        }
        let rule = rules.lock().unwrap().get(&req.path).copied();
        match rule {
            Some(404) => return Resp::status(404),
            Some(code) if !healed.load(std::sync::atomic::Ordering::SeqCst) => return Resp::status(code),
            _ => {}
        }
        let body = file_body(&req.path);
        match req.range() {
            Some("bytes=0-0") => Resp::status(206)
                .header("Content-Range", &format!("bytes 0-0/{}", body.len()))
                .header("Content-Type", "image/jpeg")
                .tap_body(vec![body[0]]),
            _ => Resp::ok(&body).header("Content-Type", "image/jpeg"),
        }
    }))
}

/// Scan, then run a full download in which `fail` maps file index -> status. Returns the job
/// outcomes by id.
fn failing_download(
    handle: &pawchive_core::session::SessionHandle,
    rx: &Receiver<UiEvent>,
    rules: &Arc<std::sync::Mutex<std::collections::HashMap<String, u16>>>,
    fail: &[(usize, u16)],
    out: &Path,
) -> Vec<(u64, bool)> {
    handle.send(Command::Scan { url: "https://pawchive.pw/fanbox/user/119251".into() });
    let files = wait_for(rx, |e| match e {
        UiEvent::ScanFinished { files, .. } => Some(files),
        _ => None,
    });
    wait_for(rx, |e| matches!(e, UiEvent::SizingFinished { .. }).then_some(()));
    for (i, code) in fail {
        rules.lock().unwrap().insert(format!("/data{}", files[*i].path), *code);
    }
    handle.send(Command::StartDownload {
        selection: Selection { categories: Category::ALL.into_iter().collect(), include_preview: false },
        custom_extensions: String::new(),
        output: out.to_path_buf(),
    });
    let mut failed = vec![];
    wait_for(rx, |e| match e {
        UiEvent::Download(pawchive_core::engine::Event::Failed { id, permanent, .. }) => {
            failed.push((id, permanent));
            None
        }
        UiEvent::DownloadFinished(_) => Some(()),
        _ => None,
    });
    failed.sort();
    failed
}

/// A 404 is permanent, so "Retry failed" must not request that file again.
#[test]
fn retry_failed_skips_permanent_failures() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let rules = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let healed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mock = flaky_site(&rt, rules.clone(), healed.clone());
    let (handle, rx) = start(&mock.base);
    let out = TempDir::new().unwrap();

    let failed = failing_download(&handle, &rx, &rules, &[(0, 404), (1, 503)], out.path());
    assert_eq!(failed, [(0, true), (1, false)]);

    healed.store(true, std::sync::atomic::Ordering::SeqCst);
    let before = mock.count();
    handle.send(Command::RetryFailed);
    let mut touched = vec![];
    let s = wait_for(&rx, |e| match e {
        UiEvent::Download(pawchive_core::engine::Event::Started { id }) => {
            touched.push(id);
            None
        }
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!(touched, [1], "only the retryable failure runs again");
    assert_eq!((s.done, s.failed), (1, 0), "{s:?}");
    assert_eq!(mock.count() - before, 1, "one request: the 503 file; the 404 file is left alone");

    // nothing retryable is left, so a second RetryFailed does nothing
    let before = mock.count();
    handle.send(Command::RetryFailed);
    wait_for(&rx, |e| matches!(e, UiEvent::Log(m) if m.contains("No failed files")).then_some(()));
    assert_eq!(mock.count(), before);
}

/// "Retry" on one row in the Problems tab re-runs just that file.
#[test]
fn retry_jobs_reruns_only_the_named_failures() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let rules = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let healed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mock = flaky_site(&rt, rules.clone(), healed.clone());
    let (handle, rx) = start(&mock.base);
    let out = TempDir::new().unwrap();

    let failed = failing_download(&handle, &rx, &rules, &[(1, 503), (2, 503), (3, 404)], out.path());
    assert_eq!(failed, [(1, false), (2, false), (3, true)]);
    healed.store(true, std::sync::atomic::Ordering::SeqCst);

    // ids that did not fail retryably (0 succeeded, 3 is a 404) are ignored
    handle.send(Command::RetryJobs(vec![2, 0, 3]));
    let mut done = vec![];
    let s = wait_for(&rx, |e| match e {
        UiEvent::Download(pawchive_core::engine::Event::Done { id, .. }) => {
            done.push(id);
            None
        }
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!((done, s.done), (vec![2], 1));

    // job 1 is still failed and can be retried afterwards
    handle.send(Command::RetryFailed);
    let s = wait_for(&rx, |e| match e {
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!(s.done, 1);
}
