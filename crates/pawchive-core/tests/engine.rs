//! Download-engine tests against an in-process mock server (loopback only, no real network).

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{range_aware, serve, Resp};
use pawchive_core::control::Control;
use pawchive_core::engine::{Engine, EngineConfig, Event, Job, Summary};
use pawchive_core::gate::Gate;
use pawchive_core::http::{Http, RetryPolicy};
use pawchive_core::settings::Settings;
use tempfile::TempDir;
use tokio::time::Instant;

fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

fn policy() -> RetryPolicy {
    RetryPolicy {
        max_tries: 4,
        base: Duration::from_millis(20),
        cap: Duration::from_secs(5),
        forbidden_pause: Duration::from_millis(300),
    }
}

fn engine(concurrency: usize, retry: RetryPolicy) -> Engine {
    let http = Http::new(&Settings::default(), None).unwrap();
    Engine::new(http, Gate::new(Duration::from_millis(1)), EngineConfig { concurrency, retry })
}

fn job(id: u64, url: String, dir: &TempDir, name: &str, expected: Option<u64>) -> Job {
    Job { id, url, dest: dir.path().join("post").join(name), expected_size: expected, label: name.into() }
}

type Log = Arc<Mutex<Vec<Event>>>;

async fn run(e: &Engine, jobs: Vec<Job>) -> (Summary, Vec<Event>) {
    run_with(e, jobs, Control::new()).await
}

async fn run_with(e: &Engine, jobs: Vec<Job>, control: Control) -> (Summary, Vec<Event>) {
    let log: Log = Arc::default();
    let l2 = log.clone();
    let s = e.run(jobs, control, move |ev| l2.lock().unwrap().push(ev)).await;
    let ev = log.lock().unwrap().clone();
    (s, ev)
}

#[tokio::test]
async fn downloads_a_file_and_leaves_no_part_file() {
    let data = body(100_000);
    let m = serve(range_aware(data.clone())).await;
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/data/a.bin"), &dir, "a.bin", None);
    let dest = j.dest.clone();
    let (s, ev) = run(&engine(2, policy()), vec![j]).await;
    assert_eq!(s, Summary { done: 1, ..Default::default() });
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!dest.with_extension("bin.part").exists());
    assert!(ev.contains(&Event::Done { id: 0, bytes: 100_000 }));
    assert!(m.hits()[0].req.range().is_none(), "no Range on a fresh download");
}

#[tokio::test]
async fn resumes_a_part_file_with_a_range_request() {
    let data = body(50_000);
    let m = serve(range_aware(data.clone())).await;
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/x"), &dir, "r.bin", Some(50_000));
    std::fs::create_dir_all(j.dest.parent().unwrap()).unwrap();
    std::fs::write(dir.path().join("post").join("r.bin.part"), &data[..12_345]).unwrap();
    let dest = j.dest.clone();
    let (s, _) = run(&engine(1, policy()), vec![j]).await;
    assert_eq!(s.done, 1, "{s:?}");
    assert_eq!(m.count(), 1);
    assert_eq!(m.hits()[0].req.range(), Some("bytes=12345-"));
    assert_eq!(std::fs::read(&dest).unwrap(), data, "resumed file must be byte-identical");
}

#[tokio::test]
async fn a_server_that_ignores_range_gets_a_clean_restart() {
    let data = body(20_000);
    let d2 = data.clone();
    let m = serve(move |_, _| Resp::ok(&d2)).await; // always 200, never 206
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/x"), &dir, "i.bin", None);
    std::fs::create_dir_all(j.dest.parent().unwrap()).unwrap();
    std::fs::write(dir.path().join("post").join("i.bin.part"), b"stale-garbage").unwrap();
    let dest = j.dest.clone();
    let (s, _) = run(&engine(1, policy()), vec![j]).await;
    assert_eq!(s.done, 1);
    assert_eq!(m.hits()[0].req.range(), Some("bytes=13-"));
    assert_eq!(std::fs::read(&dest).unwrap(), data, "stale bytes must not be spliced in");
}

#[tokio::test]
async fn a_wrong_content_range_start_restarts_instead_of_corrupting() {
    let data = body(10_000);
    let d2 = data.clone();
    let m = serve(move |req, _| match req.range() {
        // lie: claim to start at byte 0 although we were asked for byte 100
        Some(_) => Resp::status(206).header("Content-Range", "bytes 0-9999/10000").tap(|r| r.body = d2.clone()),
        None => Resp::ok(&d2),
    })
    .await;
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/x"), &dir, "w.bin", None);
    std::fs::create_dir_all(j.dest.parent().unwrap()).unwrap();
    std::fs::write(dir.path().join("post").join("w.bin.part"), &data[..100]).unwrap();
    let dest = j.dest.clone();
    let (s, _) = run(&engine(1, policy()), vec![j]).await;
    assert_eq!(s.done, 1, "{s:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&mut Self)) -> Self;
}
impl Tap for Resp {
    fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(&mut self);
        self
    }
}

#[tokio::test]
async fn skips_files_that_already_exist_with_the_expected_size() {
    let m = serve(range_aware(body(1000))).await;
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/x"), &dir, "s.bin", Some(1000));
    std::fs::create_dir_all(j.dest.parent().unwrap()).unwrap();
    std::fs::write(&j.dest, body(1000)).unwrap();
    let (s, ev) = run(&engine(1, policy()), vec![j]).await;
    assert_eq!(s, Summary { skipped: 1, ..Default::default() });
    assert_eq!(m.count(), 0, "a skipped file must not touch the network");
    assert!(ev.contains(&Event::Skipped { id: 0 }));
}

#[tokio::test]
async fn skip_without_a_known_size_requires_a_non_empty_file() {
    let m = serve(range_aware(body(500))).await;
    let dir = TempDir::new().unwrap();
    let full = job(0, m.url("/x"), &dir, "full.bin", None);
    let empty = job(1, m.url("/x"), &dir, "empty.bin", None);
    std::fs::create_dir_all(full.dest.parent().unwrap()).unwrap();
    std::fs::write(&full.dest, b"present").unwrap();
    std::fs::write(&empty.dest, b"").unwrap();
    let (e_dest, f_dest) = (empty.dest.clone(), full.dest.clone());
    let (s, _) = run(&engine(1, policy()), vec![full, empty]).await;
    assert_eq!((s.skipped, s.done), (1, 1));
    assert_eq!(std::fs::read(f_dest).unwrap(), b"present");
    assert_eq!(std::fs::read(e_dest).unwrap(), body(500));
}

#[tokio::test]
async fn a_size_mismatch_is_not_skipped_and_gets_replaced() {
    let data = body(2000);
    let m = serve(range_aware(data.clone())).await;
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/x"), &dir, "m.bin", Some(2000));
    std::fs::create_dir_all(j.dest.parent().unwrap()).unwrap();
    std::fs::write(&j.dest, b"too short").unwrap();
    let dest = j.dest.clone();
    let (s, _) = run(&engine(1, policy()), vec![j]).await;
    assert_eq!(s.done, 1);
    assert_eq!(std::fs::read(dest).unwrap(), data);
}

#[tokio::test]
async fn backs_off_on_429_honouring_retry_after() {
    let data = body(3000);
    let d2 = data.clone();
    let m = serve(move |_, n| match n {
        0 => Resp::status(429).header("Retry-After", "1"),
        1 => Resp::status(429), // no header: exponential backoff, base 20ms * 2
        _ => Resp::ok(&d2),
    })
    .await;
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/x"), &dir, "t.bin", None);
    let dest = j.dest.clone();
    let t0 = Instant::now();
    let (s, ev) = run(&engine(1, policy()), vec![j]).await;
    assert_eq!(s.done, 1, "{s:?} {ev:?}");
    assert!(t0.elapsed() >= Duration::from_secs(1), "Retry-After: 1 must be honoured, took {:?}", t0.elapsed());
    assert_eq!(m.count(), 3);
    let waits: Vec<(u32, Duration)> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Retrying { attempt, wait, .. } => Some((*attempt, *wait)),
            _ => None,
        })
        .collect();
    assert_eq!(waits, vec![(1, Duration::from_secs(1)), (2, Duration::from_millis(40))]);
    assert_eq!(std::fs::read(dest).unwrap(), data);
}

#[tokio::test]
async fn a_404_is_a_permanent_failure_with_no_retries() {
    let m = serve(|_, _| {
        Resp::status(404)
            .header("Content-Type", "application/octet-stream")
            .tap(|r| r.body = br#"{"error":"File not found"}"#.to_vec())
    })
    .await;
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/gone"), &dir, "g.bin", None);
    let dest = j.dest.clone();
    let (s, ev) = run(&engine(1, policy()), vec![j]).await;
    assert_eq!(s, Summary { failed: 1, ..Default::default() });
    assert_eq!(m.count(), 1, "404 must not be retried");
    assert!(ev.iter().any(|e| matches!(e, Event::Failed { id: 0, permanent: true, .. })), "{ev:?}");
    assert!(!ev.iter().any(|e| matches!(e, Event::Retrying { .. })));
    assert!(!dest.exists());
}

#[tokio::test]
async fn gives_up_after_max_tries_on_persistent_5xx() {
    let m = serve(|_, _| Resp::status(503)).await;
    let dir = TempDir::new().unwrap();
    let (s, ev) = run(&engine(1, policy()), vec![job(0, m.url("/x"), &dir, "e.bin", None)]).await;
    assert_eq!(s.failed, 1);
    assert_eq!(m.count(), 4, "max_tries = 4 requests in total");
    assert!(ev.iter().any(|e| matches!(e, Event::Failed { permanent: false, .. })));
}

#[tokio::test]
async fn a_403_bot_page_pauses_every_request_globally() {
    let data = body(1000);
    let d2 = data.clone();
    let m = serve(move |req, n| match (n, req.path.as_str()) {
        (0, _) => Resp::status(403),
        _ => Resp::ok(&d2),
    })
    .await;
    let dir = TempDir::new().unwrap();
    // two jobs on two workers: the second must also be held back by the global pause
    let jobs = vec![job(0, m.url("/a"), &dir, "a.bin", None), job(1, m.url("/b"), &dir, "b.bin", None)];
    let retry = RetryPolicy { forbidden_pause: Duration::from_secs(1), ..policy() };
    let (s, ev) = run(&engine(2, retry), jobs).await;
    assert_eq!(s.done, 2, "{s:?} {ev:?}");
    assert!(ev.contains(&Event::GlobalPause { secs: 1 }), "{ev:?}");
    let hits = m.hits();
    let first = hits[0].at;
    for h in &hits[1..] {
        assert!(h.at - first >= Duration::from_millis(950), "request fired {:?} after the 403", h.at - first);
    }
}

#[tokio::test]
async fn a_dropped_connection_resumes_with_a_range_request() {
    let data = body(30_000);
    let d2 = data.clone();
    let m = serve(move |req, n| match (n, req.range()) {
        // first attempt: promise 30000 bytes, deliver 10000, hang up
        (0, None) => Resp::ok(&d2[..10_000]).truncated(30_000),
        (_, Some(r)) => {
            let start: usize = r.trim_start_matches("bytes=").trim_end_matches('-').parse().unwrap();
            Resp::partial(&d2, start)
        }
        _ => Resp::ok(&d2),
    })
    .await;
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/x"), &dir, "d.bin", None);
    let dest = j.dest.clone();
    let (s, _) = run(&engine(1, policy()), vec![j]).await;
    assert_eq!(s.done, 1, "{s:?}");
    assert_eq!(m.count(), 2);
    assert_eq!(m.hits()[1].req.range(), Some("bytes=10000-"));
    assert_eq!(std::fs::read(dest).unwrap(), data);
}

#[tokio::test]
async fn never_exceeds_the_configured_concurrency() {
    let m = serve(|_, _| Resp::ok(b"x").delayed(Duration::from_millis(120))).await;
    let dir = TempDir::new().unwrap();
    let jobs: Vec<Job> = (0..6).map(|i| job(i, m.url(&format!("/{i}")), &dir, &format!("c{i}.bin"), None)).collect();
    let (s, _) = run(&engine(2, policy()), jobs).await;
    assert_eq!(s.done, 6);
    assert_eq!(m.max_inflight.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrency_is_capped_at_five() {
    let m = serve(|_, _| Resp::ok(b"x").delayed(Duration::from_millis(150))).await;
    let dir = TempDir::new().unwrap();
    let jobs: Vec<Job> = (0..12).map(|i| job(i, m.url(&format!("/{i}")), &dir, &format!("k{i}.bin"), None)).collect();
    let (s, _) = run(&engine(50, policy()), jobs).await;
    assert_eq!(s.done, 12);
    assert!(m.max_inflight.load(Ordering::SeqCst) <= 5);
}

#[tokio::test]
async fn request_starts_are_spaced_by_the_global_gate() {
    let m = serve(|_, _| Resp::ok(b"x")).await;
    let dir = TempDir::new().unwrap();
    let http = Http::new(&Settings::default(), None).unwrap();
    let e = Engine::new(http, Gate::new(Duration::from_millis(150)), EngineConfig { concurrency: 5, retry: policy() });
    let jobs: Vec<Job> = (0..5).map(|i| job(i, m.url(&format!("/{i}")), &dir, &format!("g{i}.bin"), None)).collect();
    let (s, _) = run(&e, jobs).await;
    assert_eq!(s.done, 5);
    let mut at: Vec<_> = m.hits().iter().map(|h| h.at).collect();
    at.sort();
    for w in at.windows(2) {
        assert!(w[1] - w[0] >= Duration::from_millis(130), "starts only {:?} apart", w[1] - w[0]);
    }
}

#[tokio::test]
async fn pause_stops_new_starts_until_resume() {
    let m = serve(|_, _| Resp::ok(b"hello")).await;
    let dir = TempDir::new().unwrap();
    let control = Control::new();
    control.pause();
    let jobs = vec![job(0, m.url("/x"), &dir, "p.bin", None)];
    let e = engine(1, policy());
    let c2 = control.clone();
    let h = tokio::spawn(async move { run_with(&e, jobs, c2).await });
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(m.count(), 0, "nothing may start while paused");
    control.resume();
    let (s, _) = h.await.unwrap();
    assert_eq!(s.done, 1);
}

#[tokio::test]
async fn pausing_mid_transfer_parks_the_file_and_resumes_with_range() {
    let data = body(40_000);
    let d2 = data.clone();
    // The first response delivers a prefix and then stalls with the connection open.
    let m = serve(move |req, n| match (n, req.range()) {
        (0, None) => Resp::ok(&d2[..8_000]).truncated(40_000).stalled(Duration::from_secs(10)),
        (_, Some(r)) => Resp::partial(&d2, r.trim_start_matches("bytes=").trim_end_matches('-').parse().unwrap()),
        _ => Resp::ok(&d2),
    })
    .await;
    let dir = TempDir::new().unwrap();
    let j = job(0, m.url("/x"), &dir, "pm.bin", None);
    let (dest, part) = (j.dest.clone(), dir.path().join("post").join("pm.bin.part"));
    let control = Control::new();
    let e = engine(1, policy());
    let c2 = control.clone();
    let h = tokio::spawn(async move { run_with(&e, vec![j], c2).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    control.pause();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(std::fs::metadata(&part).unwrap().len(), 8_000, "in-flight bytes are flushed and kept");
    assert!(!dest.exists());
    assert_eq!(m.count(), 1, "no new request while paused");
    control.resume();
    let (s, ev) = h.await.unwrap();
    assert_eq!(s.done, 1, "{s:?} {ev:?}");
    assert_eq!(m.hits()[1].req.range(), Some("bytes=8000-"));
    assert!(!ev.iter().any(|e| matches!(e, Event::Retrying { .. })), "pausing is not a failure");
    assert_eq!(std::fs::read(dest).unwrap(), data);
    assert!(!part.exists());
}

#[tokio::test]
async fn cancel_keeps_the_part_file_and_counts_unstarted_jobs() {
    let data = body(10_000);
    let d2 = data.clone();
    let m = serve(move |_, _| Resp::ok(&d2[..2_000]).truncated(10_000).stalled(Duration::from_secs(10))).await;
    let dir = TempDir::new().unwrap();
    let jobs = vec![
        job(0, m.url("/a"), &dir, "x0.bin", None),
        job(1, m.url("/b"), &dir, "x1.bin", None),
        job(2, m.url("/c"), &dir, "x2.bin", None),
    ];
    let control = Control::new();
    let e = engine(1, policy());
    let c2 = control.clone();
    let h = tokio::spawn(async move { run_with(&e, jobs, c2).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    control.cancel();
    let (s, ev) = h.await.unwrap();
    assert_eq!(s, Summary { cancelled: 3, ..Default::default() }, "in-flight job plus the two that never started");
    assert!(ev.iter().any(|e| matches!(e, Event::Cancelled { id: 0 })));
    assert!(!dir.path().join("post").join("x0.bin").exists());
    let kept = std::fs::metadata(dir.path().join("post").join("x0.bin.part")).unwrap().len();
    assert_eq!(kept, 2_000, "cancel keeps the .part so a later run resumes");
    assert_eq!(m.count(), 1, "the other jobs never started");
}
