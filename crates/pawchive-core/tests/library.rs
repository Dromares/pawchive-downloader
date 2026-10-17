//! The saved library, the incremental scan and "download new only", against an in-process mock
//! of the Pawchive API built from the responses saved by the DAN-1656 recon
//! (`tests/data/incremental/`). Loopback only, and every test uses its own temporary data folder.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{serve, Mock, Resp};
use pawchive_core::api::{Client, Hosts};
use pawchive_core::control::Control;
use pawchive_core::creator::CreatorRef;
use pawchive_core::engine::{Engine, EngineConfig, Event};
use pawchive_core::gate::Gate;
use pawchive_core::http::{Http, RetryPolicy};
use pawchive_core::library::{Library, PostLabel, PostState, ScanMode, ScanOutcome, ScanStep, FULL_READ_MAX_AGE_SECS};
use pawchive_core::model::Service;
use pawchive_core::settings::Settings;
use serde_json::{json, Value};
use tempfile::TempDir;

// ---- test data -----------------------------------------------------------------------------

fn data(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/incremental").join(name);
    serde_json::from_slice(&std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

fn rows(name: &str) -> Vec<Value> {
    data(name).as_array().unwrap().clone()
}

/// `fanbox 119251`: 3 posts, all with an `edited` time and downloadable files (positive control).
fn small_posts() -> Vec<Value> {
    rows("fanbox-119251-list-o0.json")
}

/// `fanbox 1386751`, first page: 50 posts, `edited` null on every one, `has_full:false` (negative
/// control: Pawchive holds no original of any of its files).
fn no_edited_posts() -> Vec<Value> {
    rows("fanbox-1386751-list-o0.json")
}

fn id_of(p: &Value) -> String {
    p["id"].as_str().unwrap().to_string()
}

// ---- the mock site --------------------------------------------------------------------------

/// What the mock site currently says. Tests change it between scans.
#[derive(Default)]
struct World {
    /// What the paged list returns, in order (may hold a repeated post or omit a live one).
    listing: Vec<Value>,
    /// Posts that answer 200 on their own URL, by id.
    live: HashMap<String, Value>,
    profile: Value,
    /// Answer this status for the list page at this offset.
    fail_page: Option<(usize, u16)>,
    /// Answer this status for this post's own URL.
    fail_post: Option<(String, u16)>,
}

impl World {
    /// The site lists exactly `posts`, and every one of them is live.
    fn set_posts(&mut self, posts: Vec<Value>) {
        self.live = posts.iter().map(|p| (id_of(p), p.clone())).collect();
        self.listing = posts;
    }

    fn set_updated(&mut self, updated: &str) {
        self.profile["updated"] = json!(updated);
    }
}

type Shared = Arc<Mutex<World>>;

fn body_of(path: &str) -> Vec<u8> {
    format!("content of {path}").into_bytes()
}

async fn site(world: Shared) -> Mock {
    serve(move |req, _| {
        let w = world.lock().unwrap();
        let (path, query) = req.path.split_once('?').unwrap_or((req.path.as_str(), ""));
        if let Some(file) = path.strip_prefix("/data") {
            return Resp::ok(&body_of(file));
        }
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        match segs.as_slice() {
            ["api", "v1", _, "user", _, "profile"] => Resp::ok(w.profile.to_string().as_bytes()),
            ["api", "v1", _, "user", _] => {
                let offset: usize = query.strip_prefix("o=").and_then(|o| o.parse().ok()).unwrap_or(0);
                if let Some((o, status)) = w.fail_page {
                    if o == offset {
                        return Resp::status(status);
                    }
                }
                let page: Vec<&Value> = w.listing.iter().skip(offset).take(50).collect();
                Resp::ok(serde_json::to_string(&page).unwrap().as_bytes())
            }
            ["api", "v1", _, "user", _, "post", id] => {
                if let Some((fid, status)) = &w.fail_post {
                    if fid == id {
                        return Resp::status(*status);
                    }
                }
                match w.live.get(*id) {
                    Some(p) => Resp::ok(p.to_string().as_bytes()),
                    None => Resp { body: br#"{"error":"Not Found"}"#.to_vec(), ..Resp::status(404) },
                }
            }
            _ => Resp::status(404),
        }
    })
    .await
}

struct Fixture {
    world: Shared,
    mock: Mock,
    lib: Library,
    client: Client,
    creator: CreatorRef,
    data_dir: TempDir,
    out_dir: TempDir,
    /// The library's clock, in unix seconds.
    clock: Arc<Mutex<i64>>,
}

const T0: i64 = 1_800_000_000;

async fn fixture(creator: CreatorRef, posts: Vec<Value>, updated: &str) -> Fixture {
    let mut w = World { profile: json!({"id": creator.id, "name": "Test", "updated": updated}), ..Default::default() };
    w.set_posts(posts);
    let world: Shared = Arc::new(Mutex::new(w));
    let mock = site(world.clone()).await;
    let data_dir = TempDir::new().unwrap();
    let clock = Arc::new(Mutex::new(T0));
    let c2 = clock.clone();
    let lib = Library::open(data_dir.path()).unwrap().with_clock(move || *c2.lock().unwrap());
    let client = Client {
        http: Http::new(&Settings::default(), None).unwrap(),
        gate: Gate::new(Duration::from_millis(1)),
        hosts: Hosts { api: mock.base.clone(), file: mock.base.clone(), thumb: mock.base.clone(), external: None },
        policy: RetryPolicy { max_tries: 1, base: Duration::from_millis(1), ..Default::default() },
    };
    Fixture { world, mock, lib, client, creator, data_dir, out_dir: TempDir::new().unwrap(), clock }
}

fn fanbox(id: &str) -> CreatorRef {
    CreatorRef { service: Service::Fanbox, id: id.into() }
}

impl Fixture {
    async fn scan(&self, mode: ScanMode) -> ScanOutcome {
        self.try_scan(mode).await.unwrap()
    }

    async fn try_scan(&self, mode: ScanMode) -> pawchive_core::Result<ScanOutcome> {
        self.lib.scan(&self.client, &self.creator, mode, &Control::new(), |_| {}).await
    }

    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap()
    }

    fn requests(&self) -> usize {
        self.mock.count()
    }

    fn engine(&self) -> Engine {
        let http = Http::new(&Settings::default(), None).unwrap();
        let retry = RetryPolicy { max_tries: 1, ..Default::default() };
        Engine::new(http, Gate::new(Duration::from_millis(1)), EngineConfig { concurrency: 2, retry })
    }

    async fn download(&self) -> pawchive_core::library::DownloadSummary {
        let events = Arc::new(Mutex::new(Vec::<Event>::new()));
        let e2 = events.clone();
        let s = self
            .lib
            .download_new(
                &self.engine(),
                &self.client.hosts,
                &self.creator,
                self.out_dir.path(),
                Control::new(),
                move |ev| e2.lock().unwrap().push(ev),
            )
            .await
            .unwrap();
        s
    }

    /// How many requests went to the file host (`/data/…`).
    fn file_requests(&self) -> Vec<String> {
        self.mock.hits().into_iter().map(|h| h.req.path).filter(|p| p.starts_with("/data/")).collect()
    }

    /// Every row of every table, as text: two equal snapshots mean the library did not change.
    fn snapshot(&self) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.lib.path()).unwrap();
        let mut out = Vec::new();
        for table in ["profiles", "posts", "files", "external_links"] {
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2, 3")).unwrap();
            let n = stmt.column_count();
            let mut rows = stmt.query([]).unwrap();
            while let Some(r) = rows.next().unwrap() {
                let cells: Vec<String> = (0..n).map(|i| format!("{:?}", r.get_ref(i).unwrap())).collect();
                out.push(format!("{table}: {}", cells.join(" | ")));
            }
        }
        out
    }
}

fn tick(f: &Fixture, secs: i64) {
    *f.clock.lock().unwrap() += secs;
}

// ---- 1. first scan, then one added, one edited, one removed ----------------------------------

/// 53 posts over two list pages: the 3 of fanbox 119251 and the 50 of fanbox 1386751.
fn two_page_posts() -> Vec<Value> {
    let mut v = small_posts();
    v.extend(no_edited_posts());
    v
}

#[tokio::test]
async fn first_scan_then_one_added_one_edited_one_removed_reports_exactly_one_of_each() {
    let f = fixture(fanbox("119251"), two_page_posts(), "2026-07-15T18:00:00").await;
    let first = f.scan(ScanMode::Update).await;
    assert_eq!((first.new_posts, first.updated_posts, first.gone_posts), (53, 0, 0));
    assert_eq!(first.pages, 2);
    assert!(first.new_files > 0);
    assert_eq!(f.lib.posts(&f.creator).unwrap().len(), 53);

    {
        let mut w = f.world();
        let mut posts = w.listing.clone();
        // removed: the second post of the small profile
        let removed = id_of(&posts.remove(1));
        // edited: the site moved `edited` forward (the text is untouched)
        posts[0]["edited"] = json!("2026-10-17T05:14:43");
        // added: a new post at the top
        let mut added = posts[0].clone();
        added["id"] = json!("99999999");
        added["title"] = json!("A brand new post");
        added["file"] = json!({"name": "new.png", "path": format!("/aa/bb/{}.png", "a".repeat(64))});
        added["attachments"] = json!([]);
        posts.insert(0, added);
        w.set_posts(posts);
        w.set_updated("2026-10-17T06:00:00");
        w.fail_post = None;
        // `removed` answers 404 on its own URL because set_posts dropped it from `live`
        assert!(!w.live.contains_key(&removed));
    }
    let before = f.requests();
    let second = f.scan(ScanMode::Update).await;
    assert_eq!(
        (second.new_posts, second.updated_posts, second.gone_posts),
        (1, 1, 1),
        "exactly 1 new, 1 updated, 1 removed: {second:?}"
    );
    assert_eq!(second.new_files, 1, "only the new post's file is new");
    let states: Vec<_> = second.changes.iter().map(|c| (c.state, c.post_id.as_str())).collect();
    assert_eq!(states.len(), 3);
    assert_eq!(states[0], (PostState::New, "99999999"));
    assert_eq!(states[1].0, PostState::Updated);
    assert_eq!(states[2].0, PostState::Gone);
    // profile + 2 list pages + the end marker + one request for the one missing post
    assert_eq!(f.requests() - before, 1 + 3 + 1);

    let posts = f.lib.posts(&f.creator).unwrap();
    assert_eq!(posts.len(), 54, "the removed post's record is kept");
    let gone: Vec<_> = posts.iter().filter(|p| p.removed_at.is_some()).collect();
    assert_eq!(gone.len(), 1);
    assert!(!gone[0].files.is_empty(), "its files stay in the library");
    assert_eq!(f.lib.profile(&f.creator).unwrap().unwrap().gone, 1);
}

// ---- 2. paging bug ---------------------------------------------------------------------------

/// DAGASI (fanbox 1549213): list pages `?o=500` and `?o=550`. Post 7503637 is the last row of the
/// first and the first row of the second; post 7503703 (same `published`) is on neither page,
/// yet answers 200 on its own URL.
fn dagasi_listing() -> Vec<Value> {
    let mut v = rows("fanbox-1549213-list-o500.json");
    v.extend(rows("fanbox-1549213-list-o550.json"));
    v
}

#[tokio::test]
async fn a_repeated_post_counts_once_and_a_hidden_live_post_is_not_marked_gone() {
    let buggy = dagasi_listing();
    let ids: Vec<String> = buggy.iter().map(id_of).collect();
    let distinct: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!((ids.len(), distinct.len()), (100, 99), "the fixture really repeats a post");
    assert!(!ids.contains(&"7503703".to_string()), "and really omits the hidden one");

    let hidden = data("fanbox-1549213-detail-7503703.json");
    let f = fixture(fanbox("1549213"), buggy.clone(), "2026-10-17T06:00:00").await;

    // Scan 1: a walk that does see the hidden post (the page alignment shifted), plus the repeat.
    {
        let mut w = f.world();
        let mut listing = buggy.clone();
        listing.insert(10, hidden.clone());
        w.set_posts(listing);
    }
    let first = f.scan(ScanMode::Update).await;
    assert_eq!(first.duplicates, 1);
    assert_eq!((first.posts_seen, first.new_posts), (100, 100), "99 distinct rows + the hidden post");
    assert_eq!(f.lib.posts(&f.creator).unwrap().len(), 100);

    // Scan 2: the buggy walk again. The hidden post is missing from it but still live.
    {
        let mut w = f.world();
        w.listing = buggy;
        w.live.insert("7503703".into(), hidden);
    }
    let second = f.scan(ScanMode::Full).await;
    assert_eq!(second.duplicates, 1, "the repeated row is seen but not counted");
    assert_eq!(second.posts_seen, 99);
    assert_eq!((second.new_posts, second.updated_posts, second.gone_posts), (0, 0, 0));
    assert_eq!(second.hidden_live, 1);
    assert!(second.changes.is_empty());
    let post = f.lib.posts(&f.creator).unwrap().into_iter().find(|p| p.post_id == "7503703").unwrap();
    assert_eq!(post.removed_at, None, "a 200 on its own URL means it is not gone");
    // The one extra request went to that post.
    let own: Vec<_> = f.mock.hits().into_iter().filter(|h| h.req.path.ends_with("/post/7503703")).collect();
    assert_eq!(own.len(), 1);
}

#[tokio::test]
async fn a_post_that_answers_404_is_gone_and_one_that_comes_back_is_restored() {
    let f = fixture(fanbox("119251"), small_posts(), "u1").await;
    f.scan(ScanMode::Update).await;
    let victim = id_of(&small_posts()[2]);
    {
        let mut w = f.world();
        let rest: Vec<Value> = small_posts().into_iter().take(2).collect();
        w.set_posts(rest);
        w.set_updated("u2");
    }
    let gone = f.scan(ScanMode::Update).await;
    assert_eq!((gone.gone_posts, gone.new_posts, gone.updated_posts), (1, 0, 0));
    assert_eq!(gone.changes[0].title, small_posts()[2]["title"].as_str().unwrap(), "the label can name the post");
    // Still missing next time: not asked about again (no per-post request), not counted again.
    f.world().set_updated("u3");
    let before = f.requests();
    let again = f.scan(ScanMode::Update).await;
    assert_eq!(again.gone_posts, 0);
    assert_eq!(f.requests() - before, 1 + 2, "profile + one list page + end marker, no post request");
    // It returns.
    f.world().set_posts(small_posts());
    f.world().set_updated("u4");
    let back = f.scan(ScanMode::Update).await;
    // A post that comes back counts as updated (ui-spec-library section 6, DAN-1661): it was
    // `(0, 0, 0)` in the first version of this test.
    assert_eq!((back.gone_posts, back.new_posts, back.updated_posts), (0, 0, 1));
    assert_eq!(back.changes.len(), 1);
    assert_eq!(back.changes[0].state, PostState::Updated);
    let p = f.lib.posts(&f.creator).unwrap().into_iter().find(|p| p.post_id == victim).unwrap();
    assert_eq!(p.removed_at, None);
}

// ---- 3. no `edited` time ---------------------------------------------------------------------

#[tokio::test]
async fn a_changed_text_is_detected_even_when_the_post_has_no_edited_time() {
    let posts = no_edited_posts();
    assert!(posts.iter().all(|p| p["edited"].is_null()), "the fixture has no edited time at all");
    let f = fixture(fanbox("1386751"), posts.clone(), "2026-06-11T00:00:00").await;
    assert_eq!(f.scan(ScanMode::Update).await.new_posts, 50);

    let changed = id_of(&posts[7]);
    {
        let mut w = f.world();
        let mut p = posts.clone();
        let text = p[7]["content"].as_str().unwrap().to_string();
        p[7]["content"] = json!(format!("{text}<p>fixed a typo</p>"));
        w.set_posts(p);
        w.set_updated("2026-10-17T06:00:00");
    }
    let second = f.scan(ScanMode::Update).await;
    assert_eq!((second.new_posts, second.updated_posts, second.gone_posts), (0, 1, 0));
    assert_eq!(second.changes.len(), 1);
    assert_eq!((second.changes[0].state, second.changes[0].post_id.as_str()), (PostState::Updated, changed.as_str()));
    let rec = f.lib.posts(&f.creator).unwrap().into_iter().find(|p| p.post_id == changed).unwrap();
    assert_eq!(rec.edited, None);
}

#[tokio::test]
async fn a_flag_flip_alone_counts_as_an_update_and_makes_the_file_available() {
    // The site finishes importing a post: has_full false -> true, `edited` untouched.
    let posts = no_edited_posts();
    let f = fixture(fanbox("1386751"), posts.clone(), "u1").await;
    f.scan(ScanMode::Update).await;
    assert!(f.lib.pending_files(&f.creator).unwrap().is_empty(), "has_full:false files are not offered");

    let mut p = posts.clone();
    p[0]["has_full"] = json!(true);
    f.world().set_posts(p);
    f.world().set_updated("u2");
    let second = f.scan(ScanMode::Update).await;
    assert_eq!((second.new_posts, second.updated_posts), (0, 1));
    assert!(!f.lib.pending_files(&f.creator).unwrap().is_empty());
}

// ---- 4. an unchanged profile costs one request -----------------------------------------------

#[tokio::test]
async fn an_unchanged_profile_costs_exactly_one_request() {
    let f = fixture(fanbox("119251"), two_page_posts(), "2026-07-15T18:00:00").await;
    f.scan(ScanMode::Update).await;
    let scanned_at = f.lib.profile(&f.creator).unwrap().unwrap().last_scanned_at;
    tick(&f, 3600);

    let before = f.requests();
    let second = f.scan(ScanMode::Update).await;
    assert_eq!(f.requests() - before, 1, "one profile request, nothing else");
    let last = f.mock.hits().last().unwrap().req.path.clone();
    assert!(last.ends_with("/user/119251/profile"), "{last}");
    assert!(second.unchanged);
    assert_eq!((second.new_posts, second.updated_posts, second.gone_posts, second.new_files), (0, 0, 0, 0));
    assert!(second.changes.is_empty());
    let rec = f.lib.profile(&f.creator).unwrap().unwrap();
    assert_eq!(rec.last_scanned_at, Some(T0 + 3600), "the scan time moves");
    assert_ne!(rec.last_scanned_at, scanned_at);
    assert_eq!(rec.last_full_read_at, Some(T0), "but not the last full read");
}

#[tokio::test]
async fn an_unchanged_profile_is_read_in_full_once_the_last_full_read_is_a_week_old() {
    let f = fixture(fanbox("119251"), small_posts(), "u1").await;
    f.scan(ScanMode::Update).await;
    tick(&f, FULL_READ_MAX_AGE_SECS - 1);
    let before = f.requests();
    assert!(f.scan(ScanMode::Update).await.unchanged);
    assert_eq!(f.requests() - before, 1);

    tick(&f, 1);
    let before = f.requests();
    let aged = f.scan(ScanMode::Update).await;
    assert!(!aged.unchanged);
    assert!(f.requests() - before > 1, "profile, list pages and the end marker");
    assert_eq!(f.lib.profile(&f.creator).unwrap().unwrap().last_full_read_at, Some(T0 + FULL_READ_MAX_AGE_SECS));
}

#[tokio::test]
async fn a_full_rescan_skips_the_gate_and_still_saves_the_profile_time() {
    let f = fixture(fanbox("119251"), small_posts(), "u1").await;
    f.scan(ScanMode::Update).await;
    let before = f.requests();
    let full = f.scan(ScanMode::Full).await;
    assert!(!full.unchanged);
    assert_eq!(f.requests() - before, 1 + 2, "profile, one list page, end marker");
    assert_eq!(full.new_posts + full.updated_posts + full.gone_posts, 0);

    // A removal the profile time never showed: a plain update scan misses it, a full rescan finds it.
    f.world().set_posts(small_posts().into_iter().take(2).collect());
    assert!(f.scan(ScanMode::Update).await.unchanged);
    let found = f.scan(ScanMode::Full).await;
    assert_eq!(found.gone_posts, 1);
}

// ---- 5. a replaced file is saved next to the old one ------------------------------------------

#[tokio::test]
async fn a_replaced_file_is_saved_beside_the_old_one_and_the_old_one_is_untouched() {
    let posts = small_posts();
    let f = fixture(fanbox("119251"), posts.clone(), "u1").await;
    f.scan(ScanMode::Update).await;
    let first = f.download().await;
    let n_files = f.file_requests().len();
    assert_eq!((first.engine.done, first.engine.failed, first.recorded), (n_files, 0, n_files));
    assert!(n_files >= 6);
    assert!(f.lib.pending_files(&f.creator).unwrap().is_empty());

    // The old cover.jpeg on disk.
    let post_id = id_of(&posts[2]);
    let old_rec = f
        .lib
        .posts(&f.creator)
        .unwrap()
        .into_iter()
        .find(|p| p.post_id == post_id)
        .unwrap()
        .files
        .into_iter()
        .find(|x| x.name == "cover.jpeg")
        .unwrap();
    let old_path = old_rec.saved_path.clone().unwrap();
    assert!(old_path.ends_with("cover.jpeg"));
    let old_bytes = std::fs::read(&old_path).unwrap();
    assert_eq!(old_bytes, body_of(&old_rec.path));

    // The creator replaces cover.jpeg: same name, new content (new hash). The rest of the post is
    // unchanged, and the post's text is edited too.
    let new_path = format!("/ab/cd/{}.jpeg", "b".repeat(64));
    {
        let mut p = posts.clone();
        p[2]["file"]["path"] = json!(new_path);
        p[2]["edited"] = json!("2026-10-17T07:00:00");
        p[2]["title"] = json!("A new title for the edited post");
        f.world().set_posts(p);
        f.world().set_updated("u2");
    }
    let scan = f.scan(ScanMode::Update).await;
    assert_eq!((scan.updated_posts, scan.new_posts), (1, 0));
    assert_eq!(scan.new_files, 1, "the replacement is the only new file");

    let pending = f.lib.pending_files(&f.creator).unwrap();
    assert_eq!(pending.len(), 1, "the other files of the edited post are skipped by hash");
    assert_eq!((pending[0].path.as_str(), pending[0].name.as_str()), (new_path.as_str(), "cover.jpeg"));
    let hits_before = f.file_requests().len();
    let second = f.download().await;
    assert_eq!((second.engine.done, second.recorded), (1, 1));
    assert_eq!(f.file_requests().len() - hits_before, 1, "only the replaced file is fetched");

    // Both exist, in the same folder, under different names; the old bytes did not change.
    let new_saved = f
        .lib
        .posts(&f.creator)
        .unwrap()
        .into_iter()
        .find(|p| p.post_id == post_id)
        .unwrap()
        .files
        .into_iter()
        .find(|x| x.path == new_path)
        .unwrap()
        .saved_path
        .unwrap();
    assert_ne!(new_saved, old_path);
    assert_eq!(new_saved.parent(), old_path.parent(), "the post keeps its folder although its title changed");
    assert_eq!(new_saved.file_name().unwrap().to_str().unwrap(), "cover (2).jpeg");
    assert_eq!(std::fs::read(&old_path).unwrap(), old_bytes, "the old file is not overwritten");
    assert_eq!(std::fs::read(&new_saved).unwrap(), body_of(&new_path));
    assert!(old_path.exists() && new_saved.exists());

    // And a third run has nothing to do.
    let hits_before = f.file_requests().len();
    let third = f.download().await;
    assert_eq!((third.engine.done, third.recorded), (0, 0));
    assert_eq!(f.file_requests().len(), hits_before);
}

#[tokio::test]
async fn a_file_whose_hash_is_downloaded_elsewhere_in_the_profile_is_skipped() {
    let mut posts = small_posts();
    // Post 2 carries post 0's cover under another name.
    let shared = posts[0]["file"].clone();
    posts[1]["attachments"] = json!([{"name": "same-content.jpeg", "path": shared["path"]}]);
    posts[1]["file"] = json!({});
    let f = fixture(fanbox("119251"), posts, "u1").await;
    f.scan(ScanMode::Update).await;
    let pending = f.lib.pending_files(&f.creator).unwrap();
    let hashes: std::collections::HashSet<_> = pending.iter().map(|p| p.hash.clone()).collect();
    assert_eq!(pending.len(), hashes.len(), "one entry per hash within a run");
    f.download().await;
    let fetched = f.file_requests();
    let unique: std::collections::HashSet<_> = fetched.iter().collect();
    assert_eq!(fetched.len(), unique.len(), "the shared file was fetched once");
    assert!(f.lib.pending_files(&f.creator).unwrap().is_empty(), "the duplicate is not pending forever");
}

#[tokio::test]
async fn files_that_pawchive_cannot_serve_are_not_offered() {
    // fanbox 1386751: has_full:false on every post. Patreon 30500811: a preview_only file.
    let f = fixture(fanbox("1386751"), no_edited_posts(), "u1").await;
    let s = f.scan(ScanMode::Update).await;
    assert_eq!(s.new_files, 0, "has_full:false files are recorded but not counted as new downloads");
    assert!(f.lib.pending_files(&f.creator).unwrap().is_empty());
    let recorded: usize = f.lib.posts(&f.creator).unwrap().iter().map(|p| p.files.len()).sum();
    assert!(recorded > 0, "they are still part of the library");
    assert_eq!(f.download().await.engine.done, 0);
    assert!(f.file_requests().is_empty());

    let preview = rows("patreon-30500811-preview-only-post.json");
    assert_eq!(preview[0]["file"]["preview_only"], json!(true));
    let creator = CreatorRef { service: Service::Patreon, id: "30500811".into() };
    let p = fixture(creator, preview, "u1").await;
    p.scan(ScanMode::Update).await;
    assert!(p.lib.pending_files(&p.creator).unwrap().is_empty());
    assert!(p.lib.posts(&p.creator).unwrap()[0].files.iter().all(|x| !x.available));
}

#[tokio::test]
async fn download_never_reuses_a_name_that_exists_on_disk() {
    let f = fixture(fanbox("119251"), small_posts(), "u1").await;
    f.scan(ScanMode::Update).await;
    // A foreign file already sits where the first download would go.
    let first = f.lib.pending_files(&f.creator).unwrap().remove(0);
    let folder = f.out_dir.path().join("fanbox_119251").join(pawchive_core::names::post_dir(
        first.post_published.as_deref(),
        &first.post_id,
        &first.post_title,
    ));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join(&first.name), b"the user's own file").unwrap();
    f.download().await;
    assert_eq!(std::fs::read(folder.join(&first.name)).unwrap(), b"the user's own file");
    let entries = std::fs::read_dir(&folder).unwrap().count();
    assert!(entries >= 2, "the download took another name");
}

// ---- 6. an interrupted scan leaves every saved row as it was ------------------------------------

/// Change the site in every way a scan notices (one added, one edited, one removed), so there is
/// something to write.
fn change_site(f: &Fixture) {
    let mut w = f.world();
    let mut posts = w.listing.clone();
    posts.remove(1);
    posts[0]["edited"] = json!("2026-10-17T05:14:43");
    let mut added = posts[5].clone();
    added["id"] = json!("88888888");
    posts.insert(0, added);
    w.set_posts(posts);
    w.set_updated("2026-10-17T06:00:00");
}

#[tokio::test]
async fn a_scan_interrupted_mid_way_leaves_every_saved_row_as_it_was() {
    let f = fixture(fanbox("119251"), two_page_posts(), "2026-07-15T18:00:00").await;
    f.scan(ScanMode::Update).await;
    f.download().await;
    let before = f.snapshot();
    assert!(before.len() > 100, "the snapshot covers all four tables: {}", before.len());
    change_site(&f);

    // (a) The site fails on the second list page, after the first one was read.
    f.world().fail_page = Some((50, 500));
    let err = f.try_scan(ScanMode::Update).await.unwrap_err();
    assert!(err.to_string().contains("500") || err.to_string().contains("gave up"), "{err}");
    assert_eq!(f.snapshot(), before, "failure after page 1");
    f.world().fail_page = None;

    // (b) The user cancels after the first page.
    let control = Control::new();
    let c2 = control.clone();
    let err = f
        .lib
        .scan(&f.client, &f.creator, ScanMode::Update, &control, move |step| {
            if matches!(step, ScanStep::Page(p) if p.pages == 1) {
                c2.cancel();
            }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, pawchive_core::Error::Cancelled), "{err}");
    assert_eq!(f.snapshot(), before, "cancel after page 1");

    // (c) The whole list was read, then the check on the removed post fails.
    let removed = id_of(&two_page_posts()[1]);
    f.world().fail_post = Some((removed, 503));
    f.try_scan(ScanMode::Update).await.unwrap_err();
    assert_eq!(f.snapshot(), before, "failure while checking the missing post");
    f.world().fail_post = None;

    // The same scan, left alone, does write everything.
    let done = f.scan(ScanMode::Update).await;
    assert_eq!((done.new_posts, done.updated_posts, done.gone_posts), (1, 1, 1));
    assert_ne!(f.snapshot(), before);
}

#[tokio::test]
async fn a_failure_while_writing_rolls_the_whole_scan_back() {
    let f = fixture(fanbox("119251"), two_page_posts(), "2026-07-15T18:00:00").await;
    f.scan(ScanMode::Update).await;
    let before = f.snapshot();
    change_site(&f);
    // Make the write itself fail halfway: any insert of a file named boom.png aborts.
    {
        let conn = rusqlite::Connection::open(f.lib.path()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON files WHEN NEW.name = 'boom.png'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    }
    {
        let mut w = f.world();
        // The last post in list order is written after the others, so earlier writes are done
        // when this one fails.
        let last = w.listing.len() - 1;
        w.listing[last]["file"] = json!({"name": "boom.png", "path": format!("/cc/dd/{}.png", "c".repeat(64))});
    }
    let err = f.try_scan(ScanMode::Update).await.unwrap_err();
    assert!(err.to_string().contains("injected failure"), "{err}");
    assert_eq!(
        f.snapshot(),
        before,
        "nothing from the failed scan survived, including the posts written before the failure"
    );
}

// ---- the store -------------------------------------------------------------------------------

#[tokio::test]
async fn the_library_survives_a_restart_and_a_removed_profile_leaves_the_files_alone() {
    let f = fixture(fanbox("119251"), small_posts(), "u1").await;
    f.scan(ScanMode::Update).await;
    f.download().await;
    let saved: Vec<PathBuf> = f
        .lib
        .posts(&f.creator)
        .unwrap()
        .iter()
        .flat_map(|p| p.files.iter().filter_map(|x| x.saved_path.clone()))
        .collect();
    assert!(!saved.is_empty());

    // Reopen the same folder.
    let again = Library::open(f.data_dir.path()).unwrap();
    let profiles = again.profiles().unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].creator, f.creator);
    assert_eq!(profiles[0].name.as_deref(), Some("Test"));
    assert_eq!(profiles[0].posts, 3);
    assert_eq!(again.posts(&f.creator).unwrap().len(), 3);
    assert!(f.data_dir.path().join("library.sqlite3").exists());

    assert!(again.remove_profile(&f.creator).unwrap());
    assert!(!again.remove_profile(&f.creator).unwrap());
    assert!(again.profiles().unwrap().is_empty());
    assert!(again.posts(&f.creator).unwrap().is_empty());
    assert!(saved.iter().all(|p| p.exists()), "downloaded files stay");
    assert_eq!(f.snapshot().len(), 0, "no rows are left behind");
}

#[test]
fn the_data_directory_is_a_parameter_and_is_created_on_open() {
    // `open` takes whatever folder it is given and never looks at the real data directory.
    let dir = TempDir::new().unwrap();
    let lib = Library::open(&dir.path().join("nested").join("data")).unwrap();
    assert!(lib.path().ends_with("nested/data/library.sqlite3"));
    assert!(lib.path().exists());
    assert!(lib.profiles().unwrap().is_empty());
}

// ---- DAN-1661: labels, change summary, steps, loading, refilling -----------------------------

fn labels(f: &Fixture) -> HashMap<String, Option<PostLabel>> {
    let at = f.lib.profile(&f.creator).unwrap().unwrap().last_change.map(|c| c.at);
    f.lib.posts(&f.creator).unwrap().into_iter().map(|p| (p.post_id.clone(), p.label(at))).collect()
}

fn count_of(l: &HashMap<String, Option<PostLabel>>, which: Option<PostLabel>) -> usize {
    l.values().filter(|x| **x == which).count()
}

/// D1: New / Updated belong to the most recent update that found a change, survive restarts and
/// updates that find nothing, and are replaced by the next update that finds something. Gone stays
/// while the post is gone; a post that comes back is Updated.
#[tokio::test]
async fn labels_belong_to_the_last_update_that_found_a_change() {
    let f = fixture(fanbox("119251"), small_posts(), "u1").await;
    f.scan(ScanMode::Update).await;
    let first = f.lib.profile(&f.creator).unwrap().unwrap();
    assert_eq!(first.last_change, None, "a first scan is a baseline, not a change");
    assert!(labels(&f).values().all(Option::is_none), "no labels after the baseline scan");

    // Update 1: one added, one edited, one removed.
    tick(&f, 100);
    let posts = small_posts();
    let (edited_id, gone_id) = (id_of(&posts[0]), id_of(&posts[1]));
    {
        let mut p = posts.clone();
        p.remove(1);
        p[0]["edited"] = json!("2026-10-17T05:14:43");
        let mut added = p[0].clone();
        added["id"] = json!("99999999");
        added["title"] = json!("Brand new");
        added["file"] = json!({"name": "new.png", "path": format!("/aa/bb/{}.png", "a".repeat(64))});
        added["attachments"] = json!([]);
        p.insert(0, added);
        f.world().set_posts(p);
        f.world().set_updated("u2");
    }
    let u1 = f.scan(ScanMode::Update).await;
    assert_eq!((u1.new_posts, u1.updated_posts, u1.gone_posts, u1.new_files), (1, 1, 1, 1));
    let rec = f.lib.profile(&f.creator).unwrap().unwrap();
    let c = rec.last_change.expect("an update that found a change");
    assert_eq!((c.new_posts, c.updated_posts, c.new_files, c.gone_posts), (1, 1, 1, 1));
    let l = labels(&f);
    assert_eq!(l["99999999"], Some(PostLabel::New));
    assert_eq!(l[&edited_id], Some(PostLabel::Updated));
    assert_eq!(l[&gone_id], Some(PostLabel::Gone));
    assert_eq!((count_of(&l, Some(PostLabel::New)), count_of(&l, Some(PostLabel::Updated))), (1, 1));

    // They survive a restart ...
    let again = Library::open(f.data_dir.path()).unwrap();
    let rec2 = again.profile(&f.creator).unwrap().unwrap();
    assert_eq!(rec2.last_change, rec.last_change);
    // ... and an update that finds nothing (quick path, then a full read that changes nothing).
    tick(&f, 100);
    assert!(f.scan(ScanMode::Update).await.unchanged);
    tick(&f, 100);
    f.world().set_updated("u3");
    let nothing = f.scan(ScanMode::Update).await;
    assert_eq!((nothing.new_posts, nothing.updated_posts, nothing.gone_posts), (0, 0, 0));
    assert_eq!(labels(&f), l, "an update that finds nothing leaves the labels alone");
    assert_eq!(f.lib.profile(&f.creator).unwrap().unwrap().last_change, rec.last_change);

    // Update 2 finds a different change: the old New / Updated labels go, Gone stays.
    tick(&f, 100);
    {
        let mut p = f.world().listing.clone();
        let last = p.len() - 1;
        p[last]["edited"] = json!("2026-10-17T09:09:09");
        f.world().set_posts(p);
        f.world().set_updated("u4");
    }
    let u2 = f.scan(ScanMode::Update).await;
    assert_eq!((u2.new_posts, u2.updated_posts, u2.gone_posts), (0, 1, 0));
    let l2 = labels(&f);
    assert_eq!(l2["99999999"], None, "the earlier New is replaced");
    assert_eq!(l2[&edited_id], None, "the earlier Updated is replaced");
    assert_eq!(l2[&gone_id], Some(PostLabel::Gone), "Gone stays for as long as the post is gone");
    assert_eq!(count_of(&l2, Some(PostLabel::Updated)), 1);

    // The post comes back: Gone clears and it is Updated.
    tick(&f, 100);
    {
        let mut p = f.world().listing.clone();
        p.push(posts[1].clone());
        f.world().set_posts(p);
        f.world().set_updated("u5");
    }
    let back = f.scan(ScanMode::Update).await;
    assert_eq!((back.new_posts, back.updated_posts, back.gone_posts), (0, 1, 0));
    assert_eq!(labels(&f)[&gone_id], Some(PostLabel::Updated));
}

/// Two scans that read the same clock second must still give the second scan's posts their own stamp.
#[tokio::test]
async fn a_second_scan_in_the_same_second_does_not_relabel_the_baseline_as_new() {
    let f = fixture(fanbox("119251"), small_posts(), "u1").await;
    f.scan(ScanMode::Update).await;
    {
        let mut p = f.world().listing.clone();
        p[1]["edited"] = json!("2026-10-17T05:14:43");
        f.world().set_posts(p);
        f.world().set_updated("u2");
    }
    f.scan(ScanMode::Update).await; // the clock never moved
    let l = labels(&f);
    assert_eq!((count_of(&l, Some(PostLabel::New)), count_of(&l, Some(PostLabel::Updated))), (0, 1), "{l:?}");
}

#[tokio::test]
async fn a_scan_reports_its_steps_in_order() {
    let f = fixture(fanbox("119251"), two_page_posts(), "u1").await;
    let steps = || {
        let log = Arc::new(Mutex::new(Vec::<ScanStep>::new()));
        let l2 = log.clone();
        (log, move |s| l2.lock().unwrap().push(s))
    };
    let run = |mode: ScanMode| {
        let (log, cb) = steps();
        let f = &f;
        async move {
            f.lib.scan(&f.client, &f.creator, mode, &Control::new(), cb).await.unwrap();
            let v = log.lock().unwrap().clone();
            v
        }
    };
    // First scan: Checking, Reading, two pages. Nothing to check afterwards.
    let first = run(ScanMode::Update).await;
    assert_eq!(first[0], ScanStep::Checking);
    assert_eq!(first[1], ScanStep::Reading);
    assert!(matches!(first[2], ScanStep::Page(p) if p.pages == 1));
    assert!(matches!(first[3], ScanStep::Page(p) if p.pages == 2));
    assert_eq!(first.len(), 4);
    // Unchanged: Checking and nothing else.
    assert_eq!(run(ScanMode::Update).await, vec![ScanStep::Checking]);
    // Full rescan: no Checking, goes straight to reading.
    let full = run(ScanMode::Full).await;
    assert_eq!(full[0], ScanStep::Reading);
    // One saved post missing from the list: Missing 0 of 1, then 1 of 1.
    {
        let mut p = f.world().listing.clone();
        p.remove(1);
        f.world().set_posts(p);
        f.world().set_updated("u2");
    }
    let missing = run(ScanMode::Update).await;
    let tail: Vec<_> = missing.iter().rev().take(2).rev().cloned().collect();
    assert_eq!(tail, vec![ScanStep::Missing { done: 0, total: 1 }, ScanStep::Missing { done: 1, total: 1 }]);
}

#[tokio::test]
async fn load_returns_the_post_list_files_and_links_as_the_screen_wants_them() {
    let f = fixture(fanbox("119251"), small_posts(), "u1").await;
    assert!(f.lib.load(&f.creator).unwrap().is_none());
    f.scan(ScanMode::Update).await;
    let before = f.lib.load(&f.creator).unwrap().unwrap();
    assert_eq!(before.posts.len(), 3);
    assert!(before.files.iter().all(|x| !x.downloaded && x.pending == !x.entry.missing));
    f.download().await;
    let after = f.lib.load(&f.creator).unwrap().unwrap();
    assert_eq!(after.files.len(), before.files.len());
    assert!(after.files.iter().all(|x| x.downloaded && !x.pending && x.saved_path.is_some()));
    assert_eq!(after.record.posts, 3);

    // Pawchive holds no copy of any file of this profile: every one comes back as missing, none pending.
    let g = fixture(fanbox("1386751"), no_edited_posts(), "u1").await;
    g.scan(ScanMode::Update).await;
    let gone = g.lib.load(&g.creator).unwrap().unwrap();
    assert!(!gone.files.is_empty());
    assert!(gone.files.iter().all(|x| x.entry.missing && !x.pending && !x.downloaded));
}

/// Download all fills a gap at the old place and never overwrites; a file that is there is left alone.
#[tokio::test]
async fn downloading_everything_refills_a_deleted_file_in_place_and_leaves_the_rest_alone() {
    let f = fixture(fanbox("119251"), small_posts(), "u1").await;
    f.scan(ScanMode::Update).await;
    f.download().await;
    let loaded = f.lib.load(&f.creator).unwrap().unwrap();
    let victim = loaded.files.iter().find(|x| x.saved_path.is_some()).unwrap().clone();
    let victim_path = victim.saved_path.clone().unwrap();
    let other = loaded.files.iter().find(|x| x.saved_path != victim.saved_path).unwrap().clone();
    let other_path = other.saved_path.clone().unwrap();
    let other_bytes = std::fs::read(&other_path).unwrap();
    std::fs::remove_file(&victim_path).unwrap();

    let keys: std::collections::HashSet<(String, String)> =
        loaded.files.iter().map(|x| (x.entry.post_id.clone(), x.entry.path.clone())).collect();
    assert!(f.lib.download_candidates(&f.creator, &keys, true).unwrap().is_empty(), "new only: nothing is new");
    let all = f.lib.download_candidates(&f.creator, &keys, false).unwrap();
    assert_eq!(all.len(), loaded.files.len());

    let mut plan = f.lib.plan_download(&f.creator, f.out_dir.path(), all).unwrap();
    for j in &mut plan.jobs {
        let t = &plan.targets[&j.id];
        j.url = f.client.hosts.full_url(&t.path);
    }
    let dests: Vec<_> = plan.jobs.iter().map(|j| j.dest.clone()).collect();
    assert!(dests.contains(&victim_path), "the gap is refilled at the old place");
    assert!(!dests.iter().any(|d| d.to_string_lossy().contains(" (2)")), "no new name for a file that is known");
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let e2 = events.clone();
    let summary = f
        .engine()
        .with_no_replace()
        .run(plan.jobs.clone(), Control::new(), move |ev| e2.lock().unwrap().push(ev))
        .await;
    assert_eq!((summary.done, summary.failed), (1, 0), "only the deleted file is fetched: {summary:?}");
    assert_eq!(summary.skipped, loaded.files.len() - 1);
    assert_eq!(std::fs::read(&victim_path).unwrap(), body_of(&victim.entry.path));
    assert_eq!(std::fs::read(&other_path).unwrap(), other_bytes);
}

// ---- DAN-1661: the session around the library ------------------------------------------------

mod session_flow {
    use super::*;
    use pawchive_core::classify::Category;
    use pawchive_core::plan::Selection;
    use pawchive_core::session::{
        spawn_with_library, Command, Loaded, SessionHandle, Tuning, UiEvent, UpdateKind, UpdateStep,
    };
    use std::sync::mpsc::Receiver;

    struct Run {
        f: Fixture,
        handle: SessionHandle,
        rx: Receiver<UiEvent>,
        url: String,
    }

    async fn run(posts: Vec<Value>) -> Run {
        let f = fixture(fanbox("119251"), posts, "u1").await;
        let lib = Library::open(f.data_dir.path()).unwrap().with_clock({
            let c = f.clock.clone();
            move || *c.lock().unwrap()
        });
        let tuning = Tuning {
            retry: RetryPolicy { max_tries: 1, base: Duration::from_millis(1), ..Default::default() },
            delay_override: Some(Duration::from_millis(1)),
        };
        let (handle, rx) =
            spawn_with_library(Settings::default(), f.client.hosts.clone(), tuning, Arc::new(|| {}), Some(Ok(lib)));
        let url = "https://pawchive.pw/fanbox/user/119251".to_string();
        Run { f, handle, rx, url }
    }

    /// Events until one matches; everything before it is returned too.
    fn until<T>(rx: &Receiver<UiEvent>, mut pick: impl FnMut(&UiEvent) -> Option<T>) -> (Vec<UiEvent>, T) {
        let mut seen = Vec::new();
        loop {
            let ev = rx.recv_timeout(Duration::from_secs(20)).expect("an event");
            if let Some(t) = pick(&ev) {
                return (seen, t);
            }
            seen.push(ev);
        }
    }

    fn loaded(rx: &Receiver<UiEvent>) -> (Vec<UiEvent>, pawchive_core::session::ProfileLoaded) {
        until(rx, |e| match e {
            UiEvent::ProfileLoaded(p) => Some((**p).clone()),
            UiEvent::ScanFailed(m) | UiEvent::UpdateFailed { cause: m, .. } => panic!("failed: {m}"),
            _ => None,
        })
    }

    fn download_all() -> Selection {
        Selection { categories: Category::ALL.into_iter().collect(), include_preview: false }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn the_list_is_announced_then_a_first_scan_saves_and_open_and_update_follow() {
        let r = run(small_posts()).await;
        let (_, list) = until(&r.rx, |e| match e {
            UiEvent::Profiles(l) => Some(l.clone()),
            _ => None,
        });
        assert_eq!(list.unwrap().len(), 0, "an empty library is announced");

        r.handle.send(Command::Scan { url: r.url.clone() });
        let (before, first) = loaded(&r.rx);
        assert!(before.iter().any(|e| matches!(e, UiEvent::ScanStarted(_))));
        assert!(!before.iter().any(|e| matches!(e, UiEvent::UpdateStarted { .. })), "a first scan is not an update");
        assert_eq!(first.how, Loaded::First);
        assert_eq!(first.record.posts, 3);
        assert_eq!(first.files.len(), first.marks.len());
        assert!(first.marks.iter().all(|m| !m.downloaded));
        let (_, list) = until(&r.rx, |e| match e {
            UiEvent::Profiles(l) => Some(l.clone()),
            _ => None,
        });
        assert_eq!(list.unwrap()[0].creator, r.f.creator);

        // Opening it again makes no request at all (the background sizing of the first scan is done).
        until(&r.rx, |e| matches!(e, UiEvent::SizingFinished { .. }).then_some(()));
        let hits = r.f.requests();
        r.handle.send(Command::OpenProfile(r.f.creator.clone()));
        let (_, opened) = loaded(&r.rx);
        assert_eq!(opened.how, Loaded::Opened);
        assert_eq!(opened.files.len(), first.files.len());
        assert_eq!(opened.probing, 0);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(r.f.requests(), hits, "Open reads the library only");

        // Updating an unchanged profile: Checking, then the quick answer, one request.
        let hits = r.f.requests();
        r.handle.send(Command::Scan { url: r.url.clone() });
        let (events, updated) = loaded(&r.rx);
        assert!(
            events.iter().any(|e| matches!(e, UiEvent::UpdateStarted { kind: UpdateKind::Update, .. })),
            "{events:?}"
        );
        assert!(events.iter().any(|e| matches!(e, UiEvent::UpdateStep(UpdateStep::Checking))));
        assert!(!events.iter().any(|e| matches!(e, UiEvent::UpdateStep(UpdateStep::Reading))));
        let Loaded::Updated(rep) = &updated.how else { panic!("{:?}", updated.how) };
        assert!(rep.unchanged);
        assert_eq!(rep.requests, 1);
        assert_eq!(r.f.requests() - hits, 1);

        // A full rescan reads the list although nothing changed.
        r.handle.send(Command::FullRescan { url: r.url.clone() });
        let (events, full) = loaded(&r.rx);
        assert!(events.iter().any(|e| matches!(e, UiEvent::UpdateStarted { kind: UpdateKind::Full, .. })));
        assert!(events.iter().any(|e| matches!(e, UiEvent::UpdateStep(UpdateStep::Page(_)))), "{events:?}");
        let Loaded::Updated(rep) = &full.how else { panic!() };
        assert!(!rep.unchanged);
        assert_eq!((rep.summary.new_posts, rep.summary.updated_posts, rep.summary.gone_posts), (0, 0, 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn download_new_fetches_only_what_is_new_and_records_it_and_all_never_replaces() {
        let r = run(small_posts()).await;
        r.handle.send(Command::Scan { url: r.url.clone() });
        let (_, first) = loaded(&r.rx);
        let n = first.files.iter().filter(|f| !f.missing).count();
        assert!(n >= 6);
        // Sizing runs in the background after a first scan; let it finish so the file requests are countable.
        until(&r.rx, |e| matches!(e, UiEvent::SizingFinished { .. }).then_some(()));
        let out = TempDir::new().unwrap();
        let go_new = |links| Command::DownloadNew {
            selection: download_all(),
            custom_extensions: String::new(),
            output: out.path().to_path_buf(),
            links,
        };
        r.handle.send(go_new(vec![]));
        let (events, summary) = until(&r.rx, |e| match e {
            UiEvent::DownloadFinished(s) => Some(s.clone()),
            _ => None,
        });
        assert!(events.iter().any(|e| matches!(e, UiEvent::DownloadStarted { jobs } if jobs.len() == n)));
        assert_eq!((summary.done, summary.failed), (n, 0), "{summary:?}");
        let (_, marks) = until(&r.rx, |e| match e {
            UiEvent::FileMarks(m) => Some(m.clone()),
            _ => None,
        });
        assert!(marks.iter().zip(&first.files).all(|(m, f)| m.downloaded == !f.missing));
        assert!(r.f.lib.pending_files(&r.f.creator).unwrap().is_empty(), "the library recorded every file");

        // A second "new only" has nothing to do.
        let hits = r.f.file_requests().len();
        r.handle.send(go_new(vec![]));
        let (_, none) = until(&r.rx, |e| match e {
            UiEvent::DownloadFinished(s) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(none.done, 0);
        assert_eq!(r.f.file_requests().len(), hits);

        // "Download all" skips what is in the folder: nothing is fetched, nothing is replaced.
        let a_file = r.f.lib.posts(&r.f.creator).unwrap()[0].files[0].saved_path.clone().unwrap();
        std::fs::write(&a_file, b"changed by the user").unwrap();
        r.handle.send(Command::StartDownload {
            selection: download_all(),
            custom_extensions: String::new(),
            output: out.path().to_path_buf(),
            links: vec![],
        });
        let (_, all) = until(&r.rx, |e| match e {
            UiEvent::DownloadFinished(s) => Some(s.clone()),
            _ => None,
        });
        assert_eq!((all.done, all.skipped), (0, n), "{all:?}");
        assert_eq!(std::fs::read(&a_file).unwrap(), b"changed by the user", "a file that is there is never replaced");
        assert_eq!(r.f.file_requests().len(), hits);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_failed_or_cancelled_update_leaves_the_library_file_byte_identical() {
        let r = run(two_page_posts()).await;
        r.handle.send(Command::Scan { url: r.url.clone() });
        loaded(&r.rx);
        until(&r.rx, |e| matches!(e, UiEvent::SizingFinished { .. }).then_some(()));
        let bytes = || std::fs::read(r.f.lib.path()).unwrap();
        let before = r.f.snapshot();
        let file_before = bytes();
        // The site starts failing on page 2.
        r.f.world().set_updated("u2");
        r.f.world().fail_page = Some((50, 500));
        r.handle.send(Command::Scan { url: r.url.clone() });
        let (_, (cause, saving)) = until(&r.rx, |e| match e {
            UiEvent::UpdateFailed { cause, saving } => Some((cause.clone(), *saving)),
            _ => None,
        });
        assert!(!cause.is_empty() && !saving);
        assert_eq!(r.f.snapshot(), before);
        assert_eq!(bytes(), file_before, "the SQLite file itself is unchanged");
        // Cancelled by the user after the first page.
        r.f.world().fail_page = None;
        r.handle.send(Command::Scan { url: r.url.clone() });
        until(&r.rx, |e| matches!(e, UiEvent::UpdateStep(UpdateStep::Page(_))).then_some(()));
        r.handle.send(Command::Cancel);
        until(&r.rx, |e| matches!(e, UiEvent::UpdateCancelled | UiEvent::ProfileLoaded(_)).then_some(()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn removing_a_profile_forgets_it_and_keeps_the_files() {
        let r = run(small_posts()).await;
        r.handle.send(Command::Scan { url: r.url.clone() });
        loaded(&r.rx);
        until(&r.rx, |e| matches!(e, UiEvent::SizingFinished { .. }).then_some(()));
        let out = TempDir::new().unwrap();
        r.handle.send(Command::DownloadNew {
            selection: download_all(),
            custom_extensions: String::new(),
            output: out.path().to_path_buf(),
            links: vec![],
        });
        until(&r.rx, |e| matches!(e, UiEvent::DownloadFinished(_)).then_some(()));
        let files: Vec<PathBuf> =
            r.f.lib
                .posts(&r.f.creator)
                .unwrap()
                .iter()
                .flat_map(|p| p.files.iter().filter_map(|x| x.saved_path.clone()))
                .collect();
        assert!(!files.is_empty());
        r.handle.send(Command::OpenProfile(r.f.creator.clone()));
        loaded(&r.rx);
        r.handle.send(Command::RemoveProfile(r.f.creator.clone()));
        until(&r.rx, |e| matches!(e, UiEvent::ProfileRemoved { .. }).then_some(()));
        assert!(r.f.lib.profiles().unwrap().is_empty());
        assert!(files.iter().all(|p| p.exists()));
        // Added again it is a first scan, and downloading saves new copies beside the old ones.
        r.handle.send(Command::Scan { url: r.url.clone() });
        let (events, again) = loaded(&r.rx);
        assert_eq!(again.how, Loaded::First);
        assert!(events.iter().any(|e| matches!(e, UiEvent::ScanStarted(_))));
        until(&r.rx, |e| matches!(e, UiEvent::SizingFinished { .. }).then_some(()));
        r.handle.send(Command::DownloadNew {
            selection: download_all(),
            custom_extensions: String::new(),
            output: out.path().to_path_buf(),
            links: vec![],
        });
        until(&r.rx, |e| matches!(e, UiEvent::DownloadFinished(_)).then_some(()));
        let copies =
            r.f.lib
                .posts(&r.f.creator)
                .unwrap()
                .iter()
                .flat_map(|p| p.files.iter().filter_map(|x| x.saved_path.clone()))
                .filter(|p| p.to_string_lossy().contains(" (2)"))
                .count();
        assert_eq!(copies, files.len(), "re-adding then downloading saves name (2) copies");
        assert!(files.iter().all(|p| p.exists()), "and the first copies are untouched");
    }
}
