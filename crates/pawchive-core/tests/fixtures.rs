//! Parser, paging and planning tests against the saved API responses in `/fixtures`.
//! Expected numbers are recomputed from the raw JSON here (with `serde_json::Value`, not the
//! crate's parser), never copied from recon.md.

mod common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{serve, Resp};
use pawchive_core::api::{Client, Hosts};
use pawchive_core::classify::{Category, CustomExts};
use pawchive_core::control::Control;
use pawchive_core::creator::{parse_creator_url, CreatorRef};
use pawchive_core::gate::Gate;
use pawchive_core::http::{Http, RetryPolicy};
use pawchive_core::model::{parse_post, parse_posts, Service};
use pawchive_core::plan::{plan_downloads, summarize, Selection};
use pawchive_core::settings::Settings;
use serde_json::Value;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name)
}

fn bytes(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn json(name: &str) -> Value {
    serde_json::from_slice(&bytes(name)).unwrap()
}

const P1: &str = "patreon-user-30500811-posts.json";
const P2: &str = "patreon-user-30500811-posts-page2.json";
const PLAST: &str = "patreon-user-30500811-posts-last.json";
const PPOST: &str = "patreon-user-30500811-post-172435214.json";
const F_BIG: &str = "fanbox-user-1386751-posts.json";
const F_SMALL: &str = "fanbox-user-119251-posts.json";
const F_SMALL_POST: &str = "fanbox-user-119251-post-12221506.json";

/// Raw `file` (if it has a path) followed by raw `attachments`, straight from the JSON.
fn raw_entries(post: &Value) -> Vec<&Value> {
    let mut v = Vec::new();
    if let Some(f) = post.get("file").filter(|f| f.get("path").and_then(Value::as_str).is_some_and(|p| !p.is_empty())) {
        v.push(f);
    }
    if let Some(a) = post.get("attachments").and_then(Value::as_array) {
        v.extend(a.iter().filter(|x| x.get("path").and_then(Value::as_str).is_some_and(|p| !p.is_empty())));
    }
    v
}

fn distinct_paths(post: &Value) -> HashSet<&str> {
    raw_entries(post).iter().map(|e| e["path"].as_str().unwrap()).collect()
}

fn posts_of(name: &str) -> Vec<Value> {
    match json(name) {
        Value::Array(a) => a,
        other => vec![other],
    }
}

// ---- paging termination -------------------------------------------------------------------

#[test]
fn the_last_page_fixture_is_an_empty_array() {
    assert_eq!(bytes(PLAST), b"[]");
    assert!(parse_posts(&bytes(PLAST)).unwrap().is_empty());
}

fn client(mock_base: &str) -> Client {
    Client {
        http: Http::new(&Settings::default(), None).unwrap(),
        gate: Gate::new(Duration::from_millis(1)),
        hosts: Hosts { api: mock_base.to_string(), file: mock_base.to_string(), thumb: mock_base.to_string() },
        policy: RetryPolicy { max_tries: 2, base: Duration::from_millis(5), ..Default::default() },
    }
}

#[tokio::test]
async fn scan_pages_by_offset_and_stops_on_the_empty_array() {
    let (p1, p2, last) = (bytes(P1), bytes(P2), bytes(PLAST));
    let m = serve(move |req, _| match req.path.as_str() {
        "/api/v1/patreon/user/30500811?o=0" => Resp::ok(&p1),
        "/api/v1/patreon/user/30500811?o=50" => Resp::ok(&p2),
        _ => Resp::ok(&last),
    })
    .await;
    let creator = parse_creator_url("https://pawchive.pw/patreon/user/30500811").unwrap();
    let mut progress = Vec::new();
    let res = client(&m.base).scan(&creator, &Control::new(), |p| progress.push(p)).await.unwrap();

    let expect_files: usize = [P1, P2].iter().flat_map(|f| posts_of(f)).map(|p| distinct_paths(&p).len()).sum();
    assert_eq!((res.pages, res.posts, res.files.len()), (2, 100, expect_files));
    let paths: Vec<_> = m.hits().into_iter().map(|h| h.req.path).collect();
    assert_eq!(
        paths,
        [
            "/api/v1/patreon/user/30500811?o=0",
            "/api/v1/patreon/user/30500811?o=50",
            "/api/v1/patreon/user/30500811?o=100"
        ],
        "two full pages, then one request that returns []"
    );
    assert_eq!(progress.len(), 2);
    assert_eq!(progress[1].posts, 100);
}

#[tokio::test]
async fn a_short_first_page_still_needs_the_empty_page_to_stop() {
    let (small, last) = (bytes(F_SMALL), bytes(PLAST));
    let m = serve(move |req, _| if req.path.ends_with("?o=0") { Resp::ok(&small) } else { Resp::ok(&last) }).await;
    let creator = parse_creator_url("https://pawchive.pw/fanbox/user/119251").unwrap();
    let res = client(&m.base).scan(&creator, &Control::new(), |_| {}).await.unwrap();
    assert_eq!((res.pages, res.posts), (1, 3));
    assert_eq!(res.files.len(), posts_of(F_SMALL).iter().map(|p| distinct_paths(p).len()).sum::<usize>());
    assert_eq!(m.count(), 2);
}

#[tokio::test]
async fn a_server_that_ignores_the_offset_cannot_loop_forever() {
    let p1 = bytes(P1);
    let m = serve(move |_, _| Resp::ok(&p1)).await;
    let creator = parse_creator_url("https://pawchive.pw/patreon/user/30500811").unwrap();
    let err = client(&m.base).scan(&creator, &Control::new(), |_| {}).await.unwrap_err().to_string();
    assert!(err.contains("repeats"), "{err}");
    assert_eq!(m.count(), 2);
}

#[tokio::test]
async fn scan_surfaces_a_404_creator_without_retrying() {
    let m = serve(|_, _| Resp::status(404)).await;
    let creator = parse_creator_url("https://pawchive.pw/patreon/user/1").unwrap();
    let err = client(&m.base).scan(&creator, &Control::new(), |_| {}).await.unwrap_err().to_string();
    assert!(err.contains("404"), "{err}");
    assert_eq!(m.count(), 1);
}

// ---- dedupe, preview_only, node, shapes ---------------------------------------------------------

#[test]
fn files_are_deduplicated_by_path_within_each_post() {
    let mut saw_duplicate = false;
    for name in [P1, P2, F_BIG, F_SMALL, PPOST, F_SMALL_POST] {
        let raw = posts_of(name);
        let parsed = parse_posts(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert_eq!(parsed.len(), raw.len(), "{name}");
        for (rp, pp) in raw.iter().zip(&parsed) {
            let files = pp.files();
            let got: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
            let uniq: HashSet<&str> = got.iter().copied().collect();
            assert_eq!(got.len(), uniq.len(), "{name} post {}: duplicate path survived", pp.id);
            assert_eq!(uniq, distinct_paths(rp), "{name} post {}", pp.id);
            saw_duplicate |= raw_entries(rp).len() != uniq.len();
        }
    }
    assert!(saw_duplicate, "fixtures must exercise at least one real duplicate (page 2 does)");
}

#[test]
fn dedupe_keeps_first_occurrence_order() {
    let p = parse_post(
        br#"{"id":"1","title":"t","file":{"name":"main.jpg","path":"/a/a.jpg"},
             "attachments":[{"name":"x.jpg","path":"/b/b.jpg"},{"name":"main-again.jpg","path":"/a/a.jpg"},{"name":"y","path":"/c/c"}]}"#,
    )
    .unwrap();
    let f = p.files();
    assert_eq!(
        f.iter().map(|f| (f.name.as_str(), f.path.as_str())).collect::<Vec<_>>(),
        [("main.jpg", "/a/a.jpg"), ("x.jpg", "/b/b.jpg"), ("y", "/c/c")]
    );
}

#[test]
fn preview_only_counts_match_the_raw_json() {
    for name in [P1, P2] {
        let raw = posts_of(name);
        let parsed = parse_posts(&serde_json::to_vec(&raw).unwrap()).unwrap();
        let want_true: usize = raw
            .iter()
            .map(|p| {
                let mut seen = HashSet::new();
                raw_entries(p)
                    .into_iter()
                    .filter(|e| seen.insert(e["path"].as_str().unwrap()))
                    .filter(|e| e["preview_only"] == Value::Bool(true))
                    .count()
            })
            .sum();
        let got_true = parsed.iter().flat_map(|p| p.files()).filter(|f| f.preview_only).count();
        assert_eq!(got_true, want_true, "{name}");
        assert!(want_true > 1000, "{name}: Patreon fixtures are mostly preview-only");
    }
}

#[test]
fn fanbox_files_have_no_preview_flag_and_are_all_downloadable() {
    for name in [F_BIG, F_SMALL, F_SMALL_POST] {
        let raw = posts_of(name);
        assert!(
            raw.iter().flat_map(raw_entries).all(|e| e.get("preview_only").is_none()),
            "{name}: fixture assumption"
        );
        let files: Vec<_> =
            parse_posts(&serde_json::to_vec(&raw).unwrap()).unwrap().iter().flat_map(|p| p.files()).collect();
        assert!(!files.is_empty());
        assert!(files.iter().all(|f| !f.preview_only && f.node.is_none()), "{name}");
    }
}

#[test]
fn the_node_field_is_tolerated_and_captured_where_present() {
    let raw = posts_of(P1);
    let want: Vec<(String, i64)> = raw
        .iter()
        .flat_map(raw_entries)
        .filter_map(|e| e.get("node").and_then(Value::as_i64).map(|n| (e["path"].as_str().unwrap().to_string(), n)))
        .collect();
    assert!(!want.is_empty(), "fixture must contain node fields");
    let files: HashMap<String, Option<i64>> =
        parse_posts(&bytes(P1)).unwrap().iter().flat_map(|p| p.files()).map(|f| (f.path, f.node)).collect();
    for (path, node) in &want {
        assert_eq!(files[path], Some(*node), "{path}");
    }
    assert_eq!(
        files.values().filter(|n| n.is_some()).count(),
        want.iter().map(|(p, _)| p).collect::<HashSet<_>>().len()
    );
}

#[test]
fn a_post_without_a_main_file_still_yields_its_attachments() {
    for name in [P1, F_BIG] {
        let raw = posts_of(name);
        let parsed = parse_posts(&serde_json::to_vec(&raw).unwrap()).unwrap();
        let hit = raw
            .iter()
            .zip(&parsed)
            .find(|(r, _)| r.get("file").and_then(|f| f.get("path")).is_none())
            .unwrap_or_else(|| panic!("{name}: expected a post whose file has no path"));
        assert_eq!(hit.1.files().len(), distinct_paths(hit.0).len());
    }
}

#[test]
fn post_detail_bodies_parse_like_list_items() {
    let detail = parse_post(&bytes(PPOST)).unwrap();
    assert_eq!(detail.id, "172435214");
    let raw = json(PPOST);
    assert_eq!(detail.files().len(), distinct_paths(&raw).len());
    let small = parse_post(&bytes(F_SMALL_POST)).unwrap();
    assert_eq!(small.files().len(), distinct_paths(&json(F_SMALL_POST)).len());
}

#[test]
fn tolerates_nulls_numeric_ids_and_unknown_fields() {
    let p = parse_post(
        br#"{"id":123,"title":null,"attachments":null,"file":null,"brand_new_field":{"x":1},"published":null}"#,
    )
    .unwrap();
    assert_eq!((p.id.as_str(), p.title.as_str()), ("123", ""));
    assert!(p.files().is_empty());
    let p = parse_post(br#"{"id":"1","file":{},"attachments":[{"path":"/a/b.png"}]}"#).unwrap();
    assert_eq!(p.files()[0].name, "b.png", "missing name falls back to the path's last segment");
}

#[test]
fn garbage_is_a_parse_error_not_a_panic() {
    assert!(parse_posts(b"<html>DDoS-Guard</html>").is_err());
    assert!(parse_posts(br#"{"error":"nope"}"#).is_err());
}

// ---- summary and planning on real shapes ---------------------------------------------------

fn all_files(names: &[&str]) -> Vec<pawchive_core::model::FileEntry> {
    names.iter().flat_map(|n| parse_posts(&bytes(n)).unwrap()).flat_map(|p| p.files()).collect()
}

fn ext(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

#[test]
fn summary_separates_preview_only_files_from_downloadable_ones() {
    let files = all_files(&[P1]);
    let s = summarize(&files, &CustomExts::default());
    let want_preview = files.iter().filter(|f| f.preview_only).count();
    assert_eq!(s.preview.count, want_preview);
    let downloadable: usize = s.per_category.values().map(|c| c.count).sum();
    assert_eq!(downloadable + want_preview, files.len());
    // on this creator every downloadable file is a zip
    assert_eq!(
        s.per_category[&Category::Archive].count,
        files.iter().filter(|f| !f.preview_only && ext(&f.name) == "zip").count()
    );
    assert!(
        !s.per_category.contains_key(&Category::Image),
        "preview-only images must not be counted as downloadable images"
    );
    // sizes are unknown until probed
    assert_eq!(s.per_category[&Category::Archive].unsized_files, s.per_category[&Category::Archive].count);
}

#[test]
fn custom_extensions_move_files_into_their_own_row() {
    let files = all_files(&[F_BIG, F_SMALL]);
    let plain = summarize(&files, &CustomExts::default());
    let custom = summarize(&files, &CustomExts::parse("png"));
    let pngs = files.iter().filter(|f| ext(&f.name) == "png").count();
    assert!(pngs > 0);
    assert_eq!(custom.per_category[&Category::Custom].count, pngs);
    assert_eq!(
        plain.per_category[&Category::Image].count - custom.per_category.get(&Category::Image).map_or(0, |c| c.count),
        pngs
    );
}

fn creator(service: Service, id: &str) -> CreatorRef {
    CreatorRef { service, id: id.into() }
}

#[test]
fn plan_places_files_in_sanitised_collision_free_post_folders() {
    let files = all_files(&[F_BIG, F_SMALL]);
    let sel = Selection { categories: Category::ALL.into_iter().collect(), include_preview: false };
    let out = Path::new("/out");
    let jobs = plan_downloads(
        &creator(Service::Fanbox, "1386751"),
        &files,
        &sel,
        &CustomExts::default(),
        out,
        &Hosts::default(),
    );
    assert_eq!(jobs.len(), files.len(), "everything is downloadable on fanbox");
    let mut dests = HashSet::new();
    for j in &jobs {
        assert!(j.dest.starts_with("/out/fanbox_1386751"), "{}", j.dest.display());
        let rel = j.dest.strip_prefix(out).unwrap();
        assert_eq!(rel.components().count(), 3, "creator/post/file: {}", rel.display());
        assert!(rel.components().all(|c| matches!(c, std::path::Component::Normal(_))));
        assert!(dests.insert(j.dest.to_string_lossy().to_lowercase()), "collision: {}", j.dest.display());
        assert!(j.url.starts_with("https://file.pawchive.pw/data/"), "{}", j.url);
    }
    let ids: Vec<u64> = jobs.iter().map(|j| j.id).collect();
    assert_eq!(ids, (0..jobs.len() as u64).collect::<Vec<_>>());
}

#[test]
fn preview_only_files_are_skipped_by_default_and_saved_as_thumbs_when_ticked() {
    let files = all_files(&[P1]);
    let want_preview = files.iter().filter(|f| f.preview_only).count();
    let c = creator(Service::Patreon, "30500811");
    let base = Selection { categories: Category::ALL.into_iter().collect(), include_preview: false };
    let without = plan_downloads(&c, &files, &base, &CustomExts::default(), Path::new("/o"), &Hosts::default());
    assert_eq!(without.len(), files.len() - want_preview);
    assert!(without.iter().all(|j| j.url.starts_with("https://file.pawchive.pw/")));

    let with = plan_downloads(
        &c,
        &files,
        &Selection { include_preview: true, ..base.clone() },
        &CustomExts::default(),
        Path::new("/o"),
        &Hosts::default(),
    );
    assert_eq!(with.len(), files.len());
    let thumbs: Vec<_> = with.iter().filter(|j| j.url.contains("/thumbnail/")).collect();
    assert_eq!(thumbs.len(), want_preview);
    assert!(thumbs.iter().all(|j| j.url.starts_with("https://img.pawchive.pw/thumbnail/data/")));
    assert!(
        thumbs.iter().all(|j| j.dest.file_name().unwrap().to_string_lossy().contains(".thumb")),
        "thumbnails can never be mistaken for originals"
    );
    assert!(thumbs.iter().all(|j| j.expected_size.is_none()));
    // ticking the preview row only adds files; the originals keep the same names
    let full_dests: HashSet<_> = without.iter().map(|j| &j.dest).collect();
    assert!(with.iter().filter(|j| !j.url.contains("/thumbnail/")).all(|j| full_dests.contains(&j.dest)));
}

#[test]
fn only_selected_categories_are_planned() {
    let files = all_files(&[P1, F_SMALL]);
    let c = creator(Service::Patreon, "30500811");
    let only_zip = Selection { categories: [Category::Archive].into_iter().collect(), include_preview: false };
    let jobs = plan_downloads(&c, &files, &only_zip, &CustomExts::default(), Path::new("/o"), &Hosts::default());
    assert!(!jobs.is_empty());
    assert!(jobs.iter().all(|j| j.dest.extension().is_some_and(|e| e == "zip")));
    let none =
        plan_downloads(&c, &files, &Selection::default(), &CustomExts::default(), Path::new("/o"), &Hosts::default());
    assert!(none.is_empty());
}

#[test]
fn planned_sizes_come_from_probes_for_full_files_only() {
    let mut files = all_files(&[F_SMALL]);
    files[0].apply_probe(Some(1234), Some("image/jpeg".into()));
    let sel = Selection { categories: Category::ALL.into_iter().collect(), include_preview: false };
    let jobs = plan_downloads(
        &creator(Service::Fanbox, "119251"),
        &files,
        &sel,
        &CustomExts::default(),
        Path::new("/o"),
        &Hosts::default(),
    );
    assert_eq!(jobs[0].expected_size, Some(1234));
    assert!(jobs[1..].iter().all(|j| j.expected_size.is_none()));
}

#[test]
fn colliding_names_within_a_post_get_numbered() {
    let p = parse_post(
        br#"{"id":"9","title":"t","published":"2024-01-02T03:04:05",
            "file":{"name":"pic.jpg","path":"/a/1.jpg"},
            "attachments":[{"name":"pic.jpg","path":"/a/2.jpg"},{"name":"PIC.jpg","path":"/a/3.jpg"}]}"#,
    )
    .unwrap();
    let sel = Selection { categories: Category::ALL.into_iter().collect(), include_preview: false };
    let jobs = plan_downloads(
        &creator(Service::Patreon, "1"),
        &p.files(),
        &sel,
        &CustomExts::default(),
        Path::new("/o"),
        &Hosts::default(),
    );
    let names: Vec<_> = jobs.iter().map(|j| j.dest.file_name().unwrap().to_string_lossy().to_string()).collect();
    assert_eq!(names, ["pic.jpg", "pic (2).jpg", "PIC (3).jpg"]);
    assert_eq!(jobs[0].dest.parent().unwrap().file_name().unwrap(), "2024-01-02_9_t");
}
