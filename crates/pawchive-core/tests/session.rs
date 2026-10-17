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
    let hosts = Hosts { api: base.into(), file: base.into(), thumb: base.into(), external: None };
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
        links: vec![],
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
        links: vec![],
    });
    let summary = wait_for(&rx, |e| match e {
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!((summary.skipped, summary.done), (files.len(), 0), "{summary:?}");
    assert_eq!(mock.count(), before);
}

/// Every file path answers 404 (the real creator 1386751 case, DAN-1540 D1).
async fn gone_site() -> common::Mock {
    let page = fixture("fanbox-user-119251-posts.json");
    serve(move |req, _| {
        if req.path.starts_with("/api/v1/fanbox/user/119251?o=0") {
            return Resp::ok(&page);
        }
        if req.path.starts_with("/api/") {
            return Resp::ok(b"[]");
        }
        Resp::status(404)
    })
    .await
}

#[test]
fn files_that_404_when_sized_are_not_offered_for_download() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(gone_site());
    let (handle, rx) = start(&mock.base);
    handle.send(Command::Scan { url: "https://pawchive.pw/fanbox/user/119251".into() });
    let files = wait_for(&rx, |e| match e {
        UiEvent::ScanFinished { files, .. } => Some(files),
        _ => None,
    });
    let to_probe = files.iter().filter(|f| !f.preview_only).count();
    assert!(to_probe > 0);
    // Not just a Log line: the front end is told which files are gone.
    let mut missing = std::collections::HashSet::new();
    wait_for(&rx, |e| match e {
        UiEvent::FileMissing { index } => {
            assert!(!files[index].preview_only);
            missing.insert(index);
            None
        }
        UiEvent::FileSized { .. } => panic!("a 404 must not count as sized"),
        UiEvent::SizingFinished { skipped: false } => Some(()),
        _ => None,
    });
    assert_eq!(missing.len(), to_probe, "every 404'd probe is reported");

    let out = TempDir::new().unwrap();
    handle.send(Command::StartDownload {
        selection: Selection { categories: Category::ALL.into_iter().collect(), include_preview: false },
        custom_extensions: String::new(),
        output: out.path().to_path_buf(),
        links: vec![],
    });
    // Files the server said are gone must not become download jobs: nothing is started at all.
    let started = wait_for(&rx, |e| match e {
        UiEvent::DownloadStarted { jobs } => Some(jobs.len()),
        UiEvent::DownloadFinished(_) => Some(0),
        _ => None,
    });
    assert_eq!(started, 0, "404'd files were queued for download");
}

/// CEO call on DAN-1543 D1: a missing image is offered as a thumbnail under Preview only (it is
/// not probed at scan time); a missing non-image is report-only (see plan.rs tests); Retry failed never reruns either.
#[test]
fn missing_images_are_offered_as_thumbnails_and_never_retried() {
    use pawchive_core::plan::missing_with_thumbnail;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(gone_site());
    let (handle, rx) = start(&mock.base);
    handle.send(Command::Scan { url: "https://pawchive.pw/fanbox/user/119251".into() });
    let files = wait_for(&rx, |e| match e {
        UiEvent::ScanFinished { files, .. } => Some(files),
        _ => None,
    });
    let mut missing = Vec::new();
    wait_for(&rx, |e| match e {
        UiEvent::FileMissing { index } => {
            missing.push(index);
            None
        }
        UiEvent::SizingFinished { skipped: false } => Some(()),
        _ => None,
    });
    let mut known = files.clone();
    for &i in &missing {
        known[i].missing = true;
    }
    let images = known.iter().filter(|f| missing_with_thumbnail(f)).count();
    assert!(images > 0, "fixture has images");

    let out = TempDir::new().unwrap();
    let start_dl = |include_preview| {
        handle.send(Command::StartDownload {
            selection: Selection { categories: Category::ALL.into_iter().collect(), include_preview },
            custom_extensions: String::new(),
            output: out.path().to_path_buf(),
            links: vec![],
        })
    };
    // Preview only unticked (the default): nothing is queued, images included.
    start_dl(false);
    let queued = wait_for(&rx, |e| match e {
        UiEvent::DownloadStarted { jobs } => Some(jobs.len()),
        UiEvent::DownloadFinished(_) => Some(0),
        _ => None,
    });
    assert_eq!(queued, 0);

    // Ticked: exactly the missing images, named `.thumb`, fetched from the thumbnail host.
    start_dl(true);
    let labels = wait_for(&rx, |e| match e {
        UiEvent::DownloadStarted { jobs } => Some(jobs.into_iter().map(|j| j.label).collect::<Vec<_>>()),
        _ => None,
    });
    assert_eq!(labels.len(), images, "{labels:?}");
    assert!(labels.iter().all(|l| l.contains(".thumb.")), "{labels:?}");
    // The thumbnails 404 here too: an ordinary permanent failure, so Retry failed has nothing to do.
    let s = wait_for(&rx, |e| match e {
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!((s.done, s.failed), (0, images), "{s:?}");
    let before = mock.count();
    handle.send(Command::RetryFailed);
    wait_for(&rx, |e| matches!(e, UiEvent::Log(m) if m.contains("No failed files")).then_some(()));
    assert_eq!(mock.count(), before, "Retry failed made requests for files that are gone");
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
        let hosts = Hosts { api: mock.base.clone(), file: mock.base.clone(), thumb: mock.base.clone(), external: None };
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
        links: vec![],
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
        links: vec![],
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

// ---- Download pressed while size probes are still running (DAN-1580) -------------------------
//
// Product call (CEO, DAN-1537): Download stays enabled while sizes are still being measured. The
// probe loop is sequential, so at most one probe is in flight when Download is pressed; it cannot
// be interrupted, so its result lands after the download started (or after it finished).

/// How the mock behaves for the pressure tests.
#[derive(Clone)]
struct Pressure {
    /// Delay on every `Range: bytes=0-0` probe response.
    probe_delay: Duration,
    /// Delay on every full-file response.
    download_delay: Duration,
    /// Full downloads of these paths answer 404 (file deleted between scan and download).
    gone: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Probes of these paths answer 404 as well.
    gone_probe: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

fn pressure_site(rt: &tokio::runtime::Runtime, p: &Pressure) -> common::Mock {
    let page = fixture("fanbox-user-119251-posts.json");
    let p = p.clone();
    rt.block_on(serve(move |req, _| {
        if req.path.starts_with("/api/v1/fanbox/user/119251?o=0") {
            return Resp::ok(&page);
        }
        if req.path.starts_with("/api/") {
            return Resp::ok(b"[]");
        }
        let body = file_body(&req.path);
        match req.range() {
            Some("bytes=0-0") => {
                if p.gone_probe.lock().unwrap().contains(&req.path) {
                    return Resp::status(404).delayed(p.probe_delay);
                }
                Resp::status(206)
                    .header("Content-Range", &format!("bytes 0-0/{}", body.len()))
                    .header("Content-Type", "image/jpeg")
                    .tap_body(vec![body[0]])
                    .delayed(p.probe_delay)
            }
            _ if p.gone.lock().unwrap().contains(&req.path) => Resp::status(404).delayed(p.download_delay),
            _ => Resp::ok(&body).header("Content-Type", "image/jpeg").delayed(p.download_delay),
        }
    }))
}

/// Everything observed from "scan, press Download after the first size, let everything settle".
struct Pressed {
    files: Vec<pawchive_core::model::FileEntry>,
    /// Indices sized before Download was pressed.
    sized_before: usize,
    jobs: Vec<pawchive_core::session::JobInfo>,
    summary: pawchive_core::engine::Summary,
    done: Vec<(u64, u64)>,
    failed: Vec<(u64, String, bool)>,
    /// `FileSized` / `FileMissing` events that arrived after `DownloadStarted`.
    late_sized: Vec<usize>,
    late_missing: Vec<usize>,
    /// The late probe result arrived before `DownloadFinished` (probe won), not after.
    probe_won: bool,
    sizing_skipped: bool,
    out: TempDir,
}

impl Pressed {
    /// Host path (`/data/..`) of file `index` as the mock sees it.
    fn url_path(&self, index: usize) -> String {
        format!("/data{}", self.files[index].path)
    }
}

/// Scan, wait for the first `FileSized` (probing is now mid-run, the next probe is slow), press
/// Download for everything, then drain events until both the download and the (cancelled) sizing
/// task have reported in. `gone_index` is marked 404-on-download once the scan has told us the
/// file list. The caller reads the mock's hit log afterwards.
fn press_download_mid_sizing(p: &Pressure, gone: &[usize], gone_probe: &[usize]) -> (Pressed, common::Mock) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = pressure_site(&rt, p);
    let (handle, rx) = start(&mock.base);
    handle.send(Command::Scan { url: "https://pawchive.pw/fanbox/user/119251".into() });
    let files = wait_for(&rx, |e| match e {
        UiEvent::ScanFinished { files, .. } => Some(files),
        UiEvent::ScanFailed(m) => panic!("scan failed: {m}"),
        _ => None,
    });
    assert!(files.len() >= 4, "fixture has enough files: {}", files.len());
    assert!(files.iter().all(|f| !f.preview_only), "every file is probed");
    for &i in gone {
        p.gone.lock().unwrap().insert(format!("/data{}", files[i].path));
    }
    for &i in gone_probe {
        p.gone_probe.lock().unwrap().insert(format!("/data{}", files[i].path));
    }
    // The first probe answers; the second is in flight (slow) when we press Download.
    wait_for(&rx, |e| matches!(e, UiEvent::FileSized { .. }).then_some(()));
    let probe_hits = || mock.hits().iter().filter(|h| h.req.range() == Some("bytes=0-0")).count();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while probe_hits() < 2 {
        assert!(std::time::Instant::now() < deadline, "second probe never started");
        std::thread::sleep(Duration::from_millis(2));
    }
    let out = TempDir::new().unwrap();
    handle.send(Command::StartDownload {
        selection: Selection { categories: Category::ALL.into_iter().collect(), include_preview: false },
        custom_extensions: String::new(),
        output: out.path().to_path_buf(),
        links: vec![],
    });

    let mut r = Pressed {
        files,
        sized_before: 1,
        jobs: vec![],
        summary: Default::default(),
        done: vec![],
        failed: vec![],
        late_sized: vec![],
        late_missing: vec![],
        probe_won: false,
        sizing_skipped: false,
        out,
    };
    let (mut finished, mut sizing_done) = (false, false);
    let mut started = false;
    wait_for(&rx, |e| {
        use pawchive_core::engine::Event as E;
        match e {
            UiEvent::DownloadStarted { jobs } => {
                started = true;
                r.jobs = jobs;
            }
            UiEvent::FileSized { index, .. } if !started => r.sized_before = r.sized_before.max(index + 1),
            UiEvent::FileSized { index, .. } => {
                r.probe_won |= !finished;
                r.late_sized.push(index)
            }
            UiEvent::FileMissing { index } => {
                r.probe_won |= !finished;
                r.late_missing.push(index)
            }
            UiEvent::Download(E::Done { id, bytes }) => r.done.push((id, bytes)),
            UiEvent::Download(E::Failed { id, error, permanent }) => r.failed.push((id, error, permanent)),
            UiEvent::DownloadFinished(s) => {
                r.summary = s;
                finished = true;
            }
            UiEvent::SizingFinished { skipped } => {
                r.sizing_skipped = skipped;
                sizing_done = true;
            }
            _ => {}
        }
        (finished && sizing_done).then_some(())
    });
    drop(handle);
    std::mem::forget(rt); // the mock's accept loop lives on this runtime; the process exits soon
    (r, mock)
}

/// Requests for `path`: (full downloads, probes).
fn requests_for(mock: &common::Mock, path: &str) -> (usize, usize) {
    let hits = mock.hits();
    let of = hits.iter().filter(|h| h.req.path == path);
    let probes = of.clone().filter(|h| h.req.range() == Some("bytes=0-0")).count();
    (of.count() - probes, probes)
}

/// The shared assertions for (a)-(d). `gone` are indices that 404 on download.
fn assert_pressure_outcome(r: &Pressed, mock: &common::Mock, gone: &[usize]) {
    let selected = r.jobs.len();
    assert_eq!(selected, r.files.len(), "every scanned file was selected");
    assert!(r.sizing_skipped, "pressing Download cancels the sizing run");

    // (c) the books balance, and nothing is left over as cancelled
    let s = &r.summary;
    assert_eq!(s.done + s.failed + s.skipped, selected, "{s:?} vs {selected} selected");
    assert_eq!((s.done, s.failed, s.skipped, s.cancelled), (selected - gone.len(), gone.len(), 0, 0), "{s:?}");

    // (a) each gone file: one permanent "404" failure, one download request (not retried)
    assert_eq!(r.failed.len(), gone.len(), "{:?}", r.failed);
    for &i in gone {
        let name = &r.files[i].name;
        // every file is selected, so job id == file index
        let hit: Vec<_> = r.failed.iter().filter(|(id, _, _)| *id == i as u64).collect();
        assert_eq!(hit.len(), 1, "{name}: {:?}", r.failed);
        let (_, error, permanent) = hit[0];
        assert!(*permanent, "a 404 is permanent: {error}");
        assert!(error.contains("HTTP 404"), "{error}");
        assert_eq!(requests_for(mock, &r.url_path(i)).0, 1, "{name}: a 404 must not be retried");
    }

    // (b) the others finished at exactly the size the server holds
    for (i, f) in r.files.iter().enumerate() {
        if gone.contains(&i) {
            continue;
        }
        let want = file_body(&r.url_path(i));
        let job = &r.jobs[i];
        assert_eq!(job.id, i as u64);
        let bytes = r.done.iter().find(|(id, _)| *id == job.id).unwrap_or_else(|| panic!("{} not done", f.name)).1;
        assert_eq!(bytes, want.len() as u64, "{}", f.name);
        let on_disk: Vec<_> = walk(r.out.path()).into_iter().filter(|p| p.ends_with(&job.label)).collect();
        assert_eq!(on_disk.len(), 1, "{}: {on_disk:?}", f.name);
        assert_eq!(std::fs::read(&on_disk[0]).unwrap(), want, "{}", f.name);
    }
    assert!(walk(r.out.path()).iter().all(|p| !p.to_string_lossy().ends_with(".part")), "no .part left");

    // (d) no file is fetched twice, whichever of probe / download finished first
    for (i, _) in r.files.iter().enumerate() {
        let (downloads, probes) = requests_for(mock, &r.url_path(i));
        assert_eq!(downloads, 1, "file {i} downloaded {downloads} times");
        assert!(probes <= 1, "file {i} probed {probes} times");
    }
    // sizing really was cut short: no probes beyond the one answered and the one in flight
    let probes: usize = (0..r.files.len()).map(|i| requests_for(mock, &r.url_path(i)).1).sum();
    assert!(probes <= r.sized_before + 1, "{probes} probes after pressing Download with {} sized", r.sized_before);
    assert!(probes < r.files.len(), "probing continued through the whole list");
}

/// Probe still in flight when the downloads finish: the late `FileSized` must change nothing.
#[test]
fn download_pressed_during_sizing_probe_finishes_after_downloads() {
    let p = Pressure {
        probe_delay: Duration::from_millis(700),
        download_delay: Duration::ZERO,
        gone: Default::default(),
        gone_probe: Default::default(),
    };
    let last = 8; // the fixture has 9 files; the last one is never probed
    let (r, mock) = press_download_mid_sizing(&p, &[last], &[]);
    assert!(r.files.len() > last);
    assert_eq!(r.files.len(), last + 1);
    assert_pressure_outcome(&r, &mock, &[last]);
    assert!(r.late_missing.is_empty(), "{:?}", r.late_missing);
    assert_eq!(r.late_sized, [1], "the in-flight probe's answer arrives after the downloads started");
}

/// Probe finishes while its file is still downloading.
#[test]
fn download_pressed_during_sizing_probe_finishes_before_downloads() {
    let p = Pressure {
        probe_delay: Duration::from_millis(50),
        download_delay: Duration::from_millis(600),
        gone: Default::default(),
        gone_probe: Default::default(),
    };
    let last = 8;
    let (r, mock) = press_download_mid_sizing(&p, &[last], &[]);
    assert_eq!(r.files.len(), last + 1);
    assert_pressure_outcome(&r, &mock, &[last]);
    assert_eq!(r.late_sized, [1], "the in-flight probe's answer arrives after Download was pressed");
    assert!(r.probe_won, "the probe was meant to answer while downloads were still running");
}

/// The in-flight probe is for a file that is gone: its 404 arrives after Download was pressed. The
/// job already exists, so the download 404s too: one permanent failure, one download request, and
/// the other files are unaffected.
#[test]
fn late_probe_404_for_a_file_already_downloading_counts_once() {
    let p = Pressure {
        probe_delay: Duration::from_millis(50),
        download_delay: Duration::from_millis(400),
        gone: Default::default(),
        gone_probe: Default::default(),
    };
    // file 1 is the probe in flight when Download is pressed
    let (r, mock) = press_download_mid_sizing(&p, &[1], &[1]);
    assert_pressure_outcome(&r, &mock, &[1]);
    assert_eq!(r.late_missing, [1], "the in-flight probe's 404 arrives after Download was pressed");
    assert!(r.late_sized.is_empty(), "{:?}", r.late_sized);
}
