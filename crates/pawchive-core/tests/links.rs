//! External links end to end (DAN-1609): post scan, Mediafire and Google Drive resolvers, download
//! sorting, per-host limits and privacy. Everything runs against the in-process mock server on
//! loopback, answering with the saved responses in `fixtures/links/` (no real network).
//!
//! The scanned post names the real hosts (`drive.google.com`, `www.mediafire.com`); `Hosts::external`
//! re-bases every request to those hosts onto the mock, keeping path and query.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{serve, Mock, Req, Resp};
use pawchive_core::api::Hosts;
use pawchive_core::classify::{classify, Category, CustomExts};
use pawchive_core::control::Control;
use pawchive_core::engine::{Engine, EngineConfig, Event, ExtSource, Job};
use pawchive_core::external::{
    self, drive, DeadReason, ExtClient, ExtHost, Notice, ResolveError, Resolved, ResolvedKind,
};
use pawchive_core::gate::Gate;
use pawchive_core::http::{Http, RetryPolicy};
use pawchive_core::links::{Host, Link, LinkKind};
use pawchive_core::plan::{JobOrigin, LinkPick, Selection};
use pawchive_core::session::{spawn, Command, SessionHandle, Tuning, UiEvent};
use pawchive_core::settings::Settings;
use tempfile::TempDir;

const POST_URL: &str = "https://pawchive.pw/patreon/user/5163822/post/162546844";
const ROOT: &str = "14tXDG5kTah5Vkbn3Pp2Z68rcCTUrRMaQ";
const MF_PATH: &str = "/REDACTED_SIGNED_TOKEN/uikej85vz8b2h7z/Super+Girl.rar";
const SUBFOLDERS: [(&str, &str, &str); 4] = [
    ("1Zxh-ANQwPd3OOm1rG0jg_maapCYrzoSU", "Jpg process", "drive-subfolder-Jpg_process.ivd.json"),
    ("1mFo_9ok2bCemS2PFWjWGGEP-ucrc89qH", "Lineart", "drive-subfolder-Lineart.ivd.json"),
    ("1KI_X3Y8K0iUvDxs6wJSSqQ3x9NryLqLx", "nsfw", "drive-subfolder-nsfw.ivd.json"),
    ("1NpEMfAmCGbS6KlH97YS3ucOGck-wBBk1", "PNG", "drive-subfolder-PNG.ivd.json"),
];

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/links").join(name)).unwrap()
}

fn text(name: &str) -> String {
    String::from_utf8(fixture(name)).unwrap()
}

/// A folder page as Drive serves it: the listing is a hex-escaped JS string.
fn folder_page(title: &str, ivd_json: &str) -> String {
    let mut esc = String::new();
    for c in ivd_json.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | ' ' | ',' | '.' | '_' | '-' => esc.push(c),
            c if (c as u32) < 0x80 => esc.push_str(&format!("\\x{:02x}", c as u32)),
            c => esc.push_str(&format!("\\u{:04x}", c as u32)),
        }
    }
    format!(
        "<html><head><title>{title} - Google Drive</title></head><body><script>window['_DRIVE_ivd'] = '{esc}';\
         if (window['_DRIVE_ivdc']) {{window['_DRIVE_ivdc']()}}</script></body></html>"
    )
}

/// A listing of `n` plain files, as the page embeds it.
fn listing(n: usize) -> String {
    let rows: Vec<String> = (0..n)
        .map(|i| format!("[\"id{i}\",[\"p\"],\"f{i}.jpg\",\"image/jpeg\",0,null,0,0,0,1,1,null,null,{}]", 100 + i))
        .collect();
    format!("[[{}],null,null,null,null,1]", rows.join(","))
}

/// Deterministic body per path.
fn body_for(key: &str) -> Vec<u8> {
    let seed = key.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    (0..(400 + seed % 300)).map(|i| (i ^ seed) as u8).collect()
}

/// Serve `body` with `Range` support and a download name.
fn file_resp(req: &Req, body: &[u8], name: &str, ct: &str) -> Resp {
    let r = match req.range().and_then(|r| r.strip_prefix("bytes=")) {
        Some("0-0") => {
            let mut r = Resp::status(206).header("Content-Range", &format!("bytes 0-0/{}", body.len()));
            r.body = vec![body[0]];
            r
        }
        Some(from) if from.ends_with('-') => {
            let start: usize = from.trim_end_matches('-').parse().unwrap_or(0);
            if start >= body.len() {
                return Resp::status(416);
            }
            Resp::partial(body, start)
        }
        _ => Resp::ok(body),
    };
    r.header("Content-Type", ct).header("Content-Disposition", &format!("attachment; filename=\"{name}\""))
}

struct Site {
    mock: Mock,
    mf_body: Vec<u8>,
}

fn drive_id(path: &str) -> Option<&str> {
    path.split("id=").nth(1).map(|r| r.split('&').next().unwrap_or(r))
}

/// pawchive.pw, Mediafire and Drive in one mock, keyed by path.
async fn site() -> Site {
    let post = fixture("post-162546844.api.json");
    let mf_page = fixture("mediafire-uikej85vz8b2h7z.file-page.html");
    let root_html = text("drive-folder-14tXDG5k.html");
    let subs: HashMap<String, String> =
        SUBFOLDERS.iter().map(|(id, name, f)| (format!("/drive/folders/{id}"), folder_page(name, &text(f)))).collect();
    let mf_body = body_for("rar");
    let mf2 = mf_body.clone();
    let mock = serve(move |req, _| {
        let p = req.path.as_str();
        if p.starts_with("/api/v1/patreon/user/5163822/post/162546844") {
            return Resp::ok(&post).header("Content-Type", "application/json");
        }
        if p.starts_with("/api/") {
            return Resp::ok(b"[]");
        }
        if p.starts_with("/data/") {
            return file_resp(req, &body_for("1.jpg"), "1.jpg", "image/jpeg");
        }
        if p.starts_with("/file/uikej85vz8b2h7z") {
            return Resp::ok(&mf_page).header("Content-Type", "text/html; charset=UTF-8");
        }
        if p == MF_PATH {
            return file_resp(req, &mf2, "Super Girl.rar", "application/x-rar");
        }
        if p.starts_with(&format!("/drive/folders/{ROOT}")) {
            return Resp::ok(root_html.as_bytes()).header("Content-Type", "text/html");
        }
        if let Some(h) = subs.get(p.split('?').next().unwrap_or(p)) {
            return Resp::ok(h.as_bytes()).header("Content-Type", "text/html");
        }
        if p.starts_with("/uc?") {
            let id = drive_id(p).unwrap_or("");
            return Resp::status(303).header("Location", &format!("/usercontent/download?id={id}&export=download"));
        }
        if p.starts_with("/usercontent/download") {
            let id = drive_id(p).unwrap_or("");
            return file_resp(req, &body_for(id), &format!("{id}.bin"), "application/octet-stream");
        }
        Resp::status(404)
    })
    .await;
    Site { mock, mf_body }
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        max_tries: 3,
        base: Duration::from_millis(10),
        cap: Duration::from_secs(1),
        forbidden_pause: Duration::from_millis(300),
    }
}

fn hosts(base: &str) -> Hosts {
    Hosts { api: base.into(), file: base.into(), thumb: base.into(), external: Some(base.into()) }
}

fn start(base: &str, settings: Settings, delay_ms: u64) -> (SessionHandle, Receiver<UiEvent>) {
    let tuning = Tuning { retry: fast_policy(), delay_override: Some(Duration::from_millis(delay_ms)) };
    spawn(settings, hosts(base), tuning, Arc::new(|| {}))
}

fn wait_for<T>(rx: &Receiver<UiEvent>, mut pick: impl FnMut(UiEvent) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
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

struct Scanned {
    files: usize,
    links: Vec<Link>,
    post_title: Option<String>,
}

fn scan(handle: &SessionHandle, rx: &Receiver<UiEvent>, url: &str) -> Scanned {
    handle.send(Command::Scan { url: url.into() });
    wait_for(rx, |e| match e {
        UiEvent::ScanFinished { files, links, post, .. } => {
            Some(Scanned { files: files.len(), links, post_title: post.map(|p| p.title) })
        }
        UiEvent::ScanFailed(m) => panic!("scan failed: {m}"),
        _ => None,
    })
}

/// Check `ids` and collect every result until the check run finishes.
fn check(
    handle: &SessionHandle,
    rx: &Receiver<UiEvent>,
    ids: Vec<usize>,
) -> (HashMap<usize, Resolved>, HashMap<usize, DeadReason>) {
    handle.send(Command::CheckLinks { links: ids });
    let (mut ok, mut dead) = (HashMap::new(), HashMap::new());
    wait_for(rx, |e| match e {
        UiEvent::LinkResolved { link, resolved } => {
            ok.insert(link, resolved);
            None
        }
        UiEvent::LinkDead { link, reason } => {
            dead.insert(link, reason);
            None
        }
        UiEvent::CheckFinished { .. } => Some(()),
        _ => None,
    });
    (ok, dead)
}

fn link_of(links: &[Link], host: Host) -> &Link {
    links.iter().find(|l| l.host == host).expect("link")
}

#[test]
fn a_post_url_scans_only_that_post_with_one_request() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let s = rt.block_on(site());
    let (handle, rx) = start(&s.mock.base, Settings::default(), 1);
    let scanned = scan(&handle, &rx, POST_URL);
    assert_eq!(scanned.files, 1, "the 1.jpg attachment");
    assert_eq!(scanned.post_title.as_deref(), Some("Super Girl"));
    assert_eq!(scanned.links.len(), 2);
    assert_eq!(scanned.links[0].host, Host::Drive);
    assert_eq!(scanned.links[1].host, Host::Mediafire);
    let api: Vec<String> = s.mock.hits().into_iter().map(|h| h.req.path).filter(|p| p.starts_with("/api/")).collect();
    assert_eq!(api, ["/api/v1/patreon/user/5163822/post/162546844"], "no creator paging");
}

#[test]
fn a_creator_url_still_scans_every_post_and_finds_links_in_the_listing() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listing = fixture("creator-5163822.list-o0.api.json");
    let mock = rt.block_on(serve(move |req, _| {
        if req.path.starts_with("/api/v1/patreon/user/5163822?o=0") {
            Resp::ok(&listing)
        } else {
            Resp::ok(b"[]")
        }
    }));
    let (handle, rx) = start(&mock.base, Settings::default(), 1);
    let scanned = scan(&handle, &rx, "https://pawchive.pw/patreon/user/5163822");
    assert!(scanned.post_title.is_none());
    assert!(scanned.links.iter().any(|l| l.host == Host::Drive));
    let paths: Vec<String> = mock.hits().into_iter().map(|h| h.req.path).filter(|p| p.starts_with("/api/")).collect();
    assert_eq!(paths, ["/api/v1/patreon/user/5163822?o=0", "/api/v1/patreon/user/5163822?o=50"]);
}

#[test]
fn checking_resolves_mediafire_and_the_twenty_file_folder() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let s = rt.block_on(site());
    let (handle, rx) = start(&s.mock.base, Settings::default(), 60);
    let scanned = scan(&handle, &rx, POST_URL);
    let (ok, dead) = check(&handle, &rx, vec![0, 1]);
    assert!(dead.is_empty(), "{dead:?}");

    let mf = &ok[&link_of(&scanned.links, Host::Mediafire).id];
    assert_eq!(mf.name, "Super Girl.rar", "the real name, not the link text");
    assert_eq!(mf.size, Some(s.mf_body.len() as u64));
    assert_eq!(mf.content_type.as_deref(), Some("application/x-rar"));

    let folder = &ok[&link_of(&scanned.links, Host::Drive).id];
    assert_eq!(folder.name, "Super Girl");
    assert_eq!(folder.file_count(), 20);
    assert_eq!(folder.size, Some(136_274_388), "the sum the recon measured");
    assert!(!folder.partial() && folder.size_complete(), "the example folder's listing is complete");
    let ResolvedKind::Folder { children, .. } = &folder.kind else { panic!("folder") };
    let paths: Vec<String> = children.iter().map(|c| c.path()).collect();
    assert_eq!(&paths[..2], ["Super Girl.jpg", "Super Girl.psd"], "the folder's own files first");
    for p in ["Jpg process/1.jpg", "Lineart/1.jpg", "nsfw/1.jpg", "PNG/4.png"] {
        assert!(paths.contains(&p.to_string()), "{p} in {paths:?}");
    }

    // One request at a time per host, spaced by the pause: root page plus four sub-folder pages.
    let drive_hits: Vec<_> = s.mock.hits().into_iter().filter(|h| h.req.path.starts_with("/drive/")).collect();
    assert_eq!(drive_hits.len(), 5);
    for w in drive_hits.windows(2) {
        assert!(w[1].at - w[0].at >= Duration::from_millis(55), "Drive requests {:?} apart", w[1].at - w[0].at);
    }
}

#[test]
fn download_saves_by_real_name_keeps_folder_paths_and_never_sends_pawchive_secrets_elsewhere() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let s = rt.block_on(site());
    let dir = TempDir::new().unwrap();
    let cookies = dir.path().join("cookies.txt");
    std::fs::write(&cookies, "127.0.0.1\tFALSE\t/\tFALSE\t0\tsession\tTOPSECRET\n").unwrap();
    let settings = Settings {
        contact: "me@example.org".into(),
        cookie_file: Some(cookies),
        delay_ms: 1000,
        ..Settings::default()
    };
    let (handle, rx) = start(&s.mock.base, settings, 1);
    let scanned = scan(&handle, &rx, POST_URL);
    let out = TempDir::new().unwrap();
    // Download the Mediafire file and the whole folder without checking them first: the download
    // checks what is still unchecked, as its first step.
    let picks = vec![
        LinkPick { link: link_of(&scanned.links, Host::Drive).id, children: None },
        LinkPick { link: link_of(&scanned.links, Host::Mediafire).id, children: None },
    ];
    handle.send(Command::StartDownload {
        selection: Selection { categories: [Category::Image].into_iter().collect(), include_preview: false },
        custom_extensions: String::new(),
        output: out.path().to_path_buf(),
        links: picks,
    });
    let mut metas: HashMap<u64, (Option<String>, Option<String>)> = HashMap::new();
    let mut infos = Vec::new();
    let summary = wait_for(&rx, |e| match e {
        UiEvent::DownloadStarted { jobs } => {
            infos = jobs;
            None
        }
        UiEvent::Download(Event::Meta { id, name, content_type }) => {
            metas.insert(id, (name, content_type));
            None
        }
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!((summary.done, summary.failed), (22, 0), "1 Pawchive image + 20 folder files + the .rar: {summary:?}");
    assert_eq!(infos.iter().filter(|j| j.origin == JobOrigin::Pawchive).count(), 1);
    assert!(infos.iter().all(|j| !j.renamed), "no clash in the example");

    let post_dir = out.path().join("patreon_5163822/2026-06-30_162546844_Super Girl");
    assert!(post_dir.join("1.jpg").is_file(), "the Pawchive image keeps its place");
    let rar = post_dir.join("Super Girl.rar");
    assert_eq!(std::fs::read(&rar).unwrap(), s.mf_body);
    for p in ["Jpg process/1.jpg", "Lineart/1.jpg", "nsfw/1.jpg", "PNG/1.png", "Super Girl/Super Girl.psd"] {
        let f = if p.starts_with("Super Girl/") { post_dir.join(p) } else { post_dir.join("Super Girl").join(p) };
        assert!(f.is_file(), "{}", f.display());
    }
    assert!(!post_dir.join("Super Girl/Jpg process/1 (2).jpg").exists(), "the three 1.jpg are in three folders");

    // Sorting uses what the server said, not the link text ("Mediafire"): a .rar is an archive.
    let rar_job = infos.iter().find(|j| j.label.ends_with("Super Girl.rar")).unwrap();
    let (name, ct) = metas[&rar_job.id].clone();
    assert_eq!(name.as_deref(), Some("Super Girl.rar"));
    assert_eq!(classify(name.as_deref().unwrap(), "", &CustomExts::default(), ct.as_deref()), Category::Archive);

    // Privacy: the cookie and the contact string go to pawchive.pw only.
    let (mut pawchive, mut external_hits) = (0, 0);
    for h in s.mock.hits() {
        let is_pawchive = h.req.path.starts_with("/api/") || h.req.path.starts_with("/data/");
        let ua = h.req.headers.get("user-agent").cloned().unwrap_or_default();
        if is_pawchive {
            pawchive += 1;
            assert!(ua.contains("me@example.org"), "{ua}");
            assert!(h.req.headers.get("cookie").is_some_and(|c| c.contains("TOPSECRET")), "{:?}", h.req.headers);
        } else {
            external_hits += 1;
            assert!(!ua.contains("example.org") && ua.starts_with("pawchive-downloader/"), "{ua}");
            assert!(!h.req.headers.contains_key("cookie"), "{}: {:?}", h.req.path, h.req.headers);
        }
    }
    assert!(pawchive >= 2 && external_hits > 20, "{pawchive} / {external_hits}");
}

#[test]
fn dead_links_come_back_dead_and_never_as_downloadable() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let post = serde_json::json!({
        "id": "9", "title": "Dead", "published": "2026-01-01T00:00:00",
        "content": "<a href=\"https://www.mediafire.com/file/zzzzzzzzzzzzzzz/gone.rar/file\">m</a>\
                    <a href=\"https://drive.google.com/drive/folders/GONEFOLDER\">d</a>\
                    <a href=\"https://drive.google.com/file/d/PRIVATEFILE/view\">p</a>\
                    <a href=\"https://mega.nz/folder/Kp3a9QxZ#k\">mega</a>",
        "file": {}, "attachments": []
    })
    .to_string();
    let mock = rt.block_on(serve(move |req, _| {
        let p = req.path.as_str();
        if p.starts_with("/api/v1/patreon/user/1/post/9") {
            Resp::ok(post.as_bytes())
        } else if p.starts_with("/file/zzzzzzzzzzzzzzz") {
            Resp::status(302).header("Location", "/error.php?errno=320&origin=download")
        } else if p.starts_with("/drive/folders/GONEFOLDER") {
            let mut r = Resp::status(404);
            r.body = fixture("drive-folder-404.html");
            r
        } else if p.starts_with("/uc?") {
            Resp::status(303).header("Location", "https://accounts.google.com/ServiceLogin?continue=x")
        } else {
            Resp::status(404)
        }
    }));
    let (handle, rx) = start(&mock.base, Settings::default(), 1);
    let scanned = scan(&handle, &rx, "https://pawchive.pw/patreon/user/1/post/9");
    assert_eq!(scanned.links.len(), 4);
    let mega = link_of(&scanned.links, Host::Mega);
    assert!(
        !mega.downloadable() && matches!(&mega.kind, LinkKind::Unsupported { why } if why == "Mega isn't supported")
    );

    let ids: Vec<usize> = scanned.links.iter().map(|l| l.id).collect();
    let (ok, dead) = check(&handle, &rx, ids);
    assert!(ok.is_empty(), "{ok:?}");
    assert_eq!(dead[&link_of(&scanned.links, Host::Mediafire).id], DeadReason::Removed);
    let drive_dead: Vec<DeadReason> =
        scanned.links.iter().filter(|l| l.host == Host::Drive).map(|l| dead[&l.id]).collect();
    assert!(drive_dead.contains(&DeadReason::Removed), "{drive_dead:?}");
    assert!(drive_dead.contains(&DeadReason::Private), "{drive_dead:?}");
    assert!(!dead.contains_key(&mega.id), "an unsupported link is never even checked");
    assert!(mock.hits().iter().all(|h| !h.req.path.contains("mega")), "nothing is requested from an unsupported host");
}

#[test]
fn drive_listing_of_fifty_or_more_entries_is_marked_possibly_incomplete() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(serve(|req, _| {
        let n = if req.path.contains("BIG") {
            50
        } else if req.path.contains("EDGE") {
            49
        } else {
            9
        };
        Resp::ok(folder_page("Folder", &listing(n)).as_bytes())
    }));
    let ext = ExtClient::new(hosts(&mock.base), Duration::from_millis(1), fast_policy()).unwrap();
    let go = |id: &str| rt.block_on(async { drive::resolve_folder(&ext, id, &Control::new(), &|_| {}).await.unwrap() });
    let (big, edge, small) = (go("BIG"), go("EDGE"), go("SMALL"));
    assert!(big.partial() && big.file_count() == 50);
    assert!(!edge.partial() && !small.partial(), "under the page limit and no marker: complete");
}

#[test]
fn an_empty_drive_folder_is_dead_and_a_large_folder_is_cut_at_a_thousand() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(serve(|req, _| {
        if req.path.contains("EMPTY") {
            Resp::ok(folder_page("Empty", "[null,null,null,null,null,1]").as_bytes())
        } else {
            // 25 sub-folders of 49 files each = 1,225 files: more than the cap.
            let id = req.path.rsplit('/').next().unwrap_or("").split('?').next().unwrap_or("");
            if id == "TOP" {
                let rows: Vec<String> = (0..25)
                    .map(|i| format!("[\"sub{i}\",[\"TOP\"],\"s{i}\",\"application/vnd.google-apps.folder\",0]"))
                    .collect();
                Resp::ok(folder_page("Top", &format!("[[{}],null,null,null,null,1]", rows.join(","))).as_bytes())
            } else {
                Resp::ok(folder_page(id, &listing(49).replace("\"id", &format!("\"{id}-"))).as_bytes())
            }
        }
    }));
    let ext = ExtClient::new(hosts(&mock.base), Duration::from_millis(1), fast_policy()).unwrap();
    let r = rt.block_on(drive::resolve_folder(&ext, "EMPTY", &Control::new(), &|_| {}));
    assert_eq!(r, Err(ResolveError::Dead(DeadReason::Empty)));
    let big = rt.block_on(drive::resolve_folder(&ext, "TOP", &Control::new(), &|_| {})).unwrap();
    assert_eq!(big.file_count(), drive::MAX_FILES);
    assert!(big.partial(), "cut at the cap, so it says so");
}

#[test]
fn the_virus_scan_page_is_passed_by_submitting_every_hidden_field() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let confirm = fixture("drive-virus-scan-confirm.html");
    let data = body_for("celeba");
    let d2 = data.clone();
    let mock = rt.block_on(serve(move |req, _| {
        let p = req.path.as_str();
        if p.starts_with("/uc?export=download&id=0B7EVK8r0v71pZjFTYXZWM3FlRnM") {
            Resp::status(303)
                .header("Location", "/usercontent/download?id=0B7EVK8r0v71pZjFTYXZWM3FlRnM&export=download")
        } else if p.contains("confirm=t") && p.contains("uuid=") {
            file_resp(req, &d2, "img_align_celeba.zip", "application/octet-stream")
        } else if p.starts_with("/usercontent/download") || p.starts_with("/download") {
            Resp::ok(&confirm).header("Content-Type", "text/html; charset=utf-8")
        } else {
            Resp::status(404)
        }
    }));
    let ext = ExtClient::new(hosts(&mock.base), Duration::from_millis(1), fast_policy()).unwrap();
    let r = rt.block_on(drive::resolve_file(&ext, "0B7EVK8r0v71pZjFTYXZWM3FlRnM", &Control::new(), &|_| {})).unwrap();
    assert_eq!(r.name, "img_align_celeba.zip");
    assert_eq!(r.size, Some(data.len() as u64));
    let last = mock.hits().last().unwrap().req.path.clone();
    for field in
        ["id=0B7EVK8r0v71pZjFTYXZWM3FlRnM", "export=download", "confirm=t", "uuid=00000000-0000-0000-0000-000000000000"]
    {
        assert!(last.contains(field), "{field} missing from {last}");
    }
    assert!(last.starts_with("/download?"), "the form's action, not the first URL: {last}");
}

#[test]
fn a_drive_throttle_cools_down_drive_only_and_the_file_fails_retryably() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let post = serde_json::json!({
        "id": "9", "title": "Quota", "published": "2026-01-01T00:00:00",
        "content": "<a href=\"https://drive.google.com/file/d/QUOTAFILE/view\">d</a>\
                    <a href=\"https://www.mediafire.com/file/uikej85vz8b2h7z/Super_Girl.rar/file\">m</a>",
        "file": {"name": "1.jpg", "path": "/aa/bb/1.jpg"}, "attachments": []
    })
    .to_string();
    let mf_page = fixture("mediafire-uikej85vz8b2h7z.file-page.html");
    let mf_body = body_for("rar");
    let mf2 = mf_body.clone();
    let mock = rt.block_on(serve(move |req, _| {
        let p = req.path.as_str();
        if p.starts_with("/api/v1/patreon/user/1/post/9") {
            Resp::ok(post.as_bytes())
        } else if p.starts_with("/data/") {
            file_resp(req, &body_for("1.jpg"), "1.jpg", "image/jpeg")
        } else if p.starts_with("/file/uikej85vz8b2h7z") {
            Resp::ok(&mf_page).header("Content-Type", "text/html")
        } else if p == MF_PATH {
            file_resp(req, &mf2, "Super Girl.rar", "application/x-rar")
        } else if p.starts_with("/uc?") {
            Resp::status(429)
        } else {
            Resp::status(404)
        }
    }));
    let (handle, rx) = start(&mock.base, Settings::default(), 1);
    let scanned = scan(&handle, &rx, "https://pawchive.pw/patreon/user/1/post/9");
    // Check Mediafire first so only the Drive file is resolved at download time (it cannot be:
    // the throttle is what this test is about, so pick the Drive file after a check that fails).
    let mf = link_of(&scanned.links, Host::Mediafire).id;
    let dv = link_of(&scanned.links, Host::Drive).id;
    let (ok, _) = check(&handle, &rx, vec![mf]);
    assert!(ok.contains_key(&mf));

    // Drive's throttle answers the check itself: the row stays unchecked ("Couldn't check"), a
    // cool-down event names Drive, and the Mediafire download still goes through.
    handle.send(Command::CheckLinks { links: vec![dv] });
    let mut cooled = Vec::new();
    let failure = wait_for(&rx, |e| match e {
        UiEvent::HostCooldown { host, secs } => {
            cooled.push((host, secs));
            None
        }
        UiEvent::LinkCheckFailed { link, message } if link == dv => Some(message),
        _ => None,
    });
    assert!(failure.is_some_and(|m| m.contains("gave up")), "a throttled check is a transient failure");
    assert!(!cooled.is_empty(), "Drive's 429 starts a cool-down");
    assert!(cooled.iter().all(|(h, _)| *h == ExtHost::Drive), "only Drive cools down: {cooled:?}");

    let out = TempDir::new().unwrap();
    handle.send(Command::StartDownload {
        selection: Selection { categories: [Category::Image].into_iter().collect(), include_preview: false },
        custom_extensions: String::new(),
        output: out.path().to_path_buf(),
        links: vec![LinkPick { link: mf, children: None }],
    });
    let summary = wait_for(&rx, |e| match e {
        UiEvent::DownloadFinished(s) => Some(s),
        _ => None,
    });
    assert_eq!((summary.done, summary.failed), (2, 0), "Pawchive image + Mediafire file went through: {summary:?}");
}

/// Three Drive files, concurrency 3: the engine still runs one Drive download at a time, and a
/// Pawchive-side job is not held up behind them.
#[test]
fn at_most_one_download_per_external_host_at_a_time() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(serve(|req, _| {
        let p = req.path.as_str();
        if p.starts_with("/uc?") {
            let id = drive_id(p).unwrap_or("");
            Resp::status(303).header("Location", &format!("/usercontent/download?id={id}&export=download"))
        } else if p.starts_with("/usercontent/") {
            file_resp(req, &body_for(p), "x.bin", "application/octet-stream").delayed(Duration::from_millis(150))
        } else {
            Resp::status(404)
        }
    }));
    let dir = TempDir::new().unwrap();
    let ext = ExtClient::new(hosts(&mock.base), Duration::from_millis(1), fast_policy()).unwrap();
    let http = Http::new(&Settings::default(), None).unwrap();
    let engine =
        Engine::new(http, Gate::new(Duration::from_millis(1)), EngineConfig { concurrency: 3, retry: fast_policy() })
            .with_external(ext);
    let jobs: Vec<Job> = (0..3)
        .map(|i| Job {
            id: i,
            url: String::new(),
            dest: dir.path().join(format!("f{i}.bin")),
            expected_size: None,
            label: format!("p/f{i}.bin"),
            ext: Some(ExtSource::Drive { file_id: format!("ID{i}") }),
        })
        .collect();
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let e2 = events.clone();
    let summary = rt.block_on(engine.run(jobs, Control::new(), move |e| e2.lock().unwrap().push(e)));
    assert_eq!(summary.done, 3);
    assert_eq!(mock.max_inflight.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn a_drive_file_that_is_gone_or_private_fails_permanently_with_its_reason() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(serve(|req, _| {
        if req.path.contains("GONE") {
            Resp::status(404)
        } else {
            Resp::status(303).header("Location", "https://accounts.google.com/ServiceLogin")
        }
    }));
    let dir = TempDir::new().unwrap();
    let ext = ExtClient::new(hosts(&mock.base), Duration::from_millis(1), fast_policy()).unwrap();
    let engine = Engine::new(
        Http::new(&Settings::default(), None).unwrap(),
        Gate::new(Duration::from_millis(1)),
        EngineConfig { concurrency: 2, retry: fast_policy() },
    )
    .with_external(ext);
    let job = |i: u64, id: &str| Job {
        id: i,
        url: String::new(),
        dest: dir.path().join(format!("f{i}")),
        expected_size: None,
        label: format!("p/f{i}"),
        ext: Some(ExtSource::Drive { file_id: id.into() }),
    };
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let e2 = events.clone();
    rt.block_on(
        engine.run(vec![job(0, "GONE"), job(1, "SECRET")], Control::new(), move |e| e2.lock().unwrap().push(e)),
    );
    let ev = events.lock().unwrap().clone();
    let failed = |id| {
        ev.iter()
            .find_map(|e| match e {
                Event::Failed { id: i, error, permanent } if *i == id => Some((error.clone(), *permanent)),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(failed(0), ("File removed on Google Drive".to_string(), true));
    assert_eq!(failed(1), ("Not shared publicly".to_string(), true));
}

#[test]
fn engine_reports_a_cooldown_per_host_and_keeps_pawchive_going() {
    // A Drive file that always answers 429 while a Pawchive-style job runs beside it.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(serve(|req, _| {
        if req.path.starts_with("/uc?") {
            Resp::status(429)
        } else {
            file_resp(req, &body_for("p"), "p.bin", "application/octet-stream")
        }
    }));
    let dir = TempDir::new().unwrap();
    let ext = ExtClient::new(hosts(&mock.base), Duration::from_millis(1), fast_policy()).unwrap();
    let engine = Engine::new(
        Http::new(&Settings::default(), None).unwrap(),
        Gate::new(Duration::from_millis(1)),
        EngineConfig { concurrency: 2, retry: fast_policy() },
    )
    .with_external(ext);
    let jobs = vec![
        Job {
            id: 0,
            url: String::new(),
            dest: dir.path().join("drive.bin"),
            expected_size: None,
            label: "p/drive.bin".into(),
            ext: Some(ExtSource::Drive { file_id: "Q".into() }),
        },
        Job {
            id: 1,
            url: mock.url("/data/p.bin"),
            dest: dir.path().join("p.bin"),
            expected_size: None,
            label: "p/p.bin".into(),
            ext: None,
        },
    ];
    let events: Arc<Mutex<Vec<(std::time::Instant, Event)>>> = Arc::default();
    let e2 = events.clone();
    let summary =
        rt.block_on(engine.run(jobs, Control::new(), move |e| e2.lock().unwrap().push((std::time::Instant::now(), e))));
    assert_eq!((summary.done, summary.failed), (1, 1));
    let ev = events.lock().unwrap().clone();
    assert!(ev.iter().any(|(_, e)| matches!(e, Event::ExtCooldown { host: ExtHost::Drive, .. })));
    assert!(!ev.iter().any(|(_, e)| matches!(e, Event::GlobalPause { .. })), "the Pawchive pause is untouched");
    let (t_done, _) = ev.iter().find(|(_, e)| matches!(e, Event::Done { id: 1, .. })).unwrap();
    let (t_fail, f) = ev.iter().find(|(_, e)| matches!(e, Event::Failed { id: 0, .. })).unwrap();
    assert!(t_done < t_fail, "the Pawchive file finished while Drive was cooling down");
    match f {
        Event::Failed { error, permanent, .. } => {
            assert!(!permanent, "a download limit is worth retrying later");
            assert_eq!(error, "Download limit reached on Google Drive. Try again later.");
        }
        _ => unreachable!(),
    }
}

#[test]
fn mediafire_resolution_uses_the_signed_link_at_download_time_and_never_keeps_it() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let s = rt.block_on(site());
    let ext = ExtClient::new(hosts(&s.mock.base), Duration::from_millis(1), fast_policy()).unwrap();
    let link = Link {
        id: 0,
        url: "https://www.mediafire.com/file/uikej85vz8b2h7z/Super_Girl.rar/file".into(),
        host: Host::Mediafire,
        kind: LinkKind::MediafireFile {
            key: "uikej85vz8b2h7z".into(),
            page_url: "https://www.mediafire.com/file/uikej85vz8b2h7z/Super_Girl.rar/file".into(),
        },
        name_hint: Some("Super_Girl.rar".into()),
        posts: vec![],
    };
    let notices: Arc<Mutex<Vec<Notice>>> = Arc::default();
    let n2 = notices.clone();
    let r = rt.block_on(external::resolve(&ext, &link, &Control::new(), &move |n| n2.lock().unwrap().push(n))).unwrap();
    assert_eq!(r.name, "Super Girl.rar");
    // The resolved record carries no URL at all, so there is nothing to store or reuse.
    assert!(!format!("{r:?}").contains("REDACTED_SIGNED_TOKEN"));
    let paths: Vec<String> = s.mock.hits().into_iter().map(|h| h.req.path).collect();
    assert_eq!(paths, ["/file/uikej85vz8b2h7z/Super_Girl.rar/file", MF_PATH]);
}
