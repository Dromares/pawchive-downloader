//! The update scan and the full rescan.
//!
//! 1. **Gate** (update scans only): one profile request. If its `updated` equals the saved value
//!    and the last full read is under [`FULL_READ_MAX_AGE_SECS`] old, the answer is "0 new, 0
//!    updated" and nothing else is fetched.
//! 2. **Walk**: read the whole post list (`?o=0,50,…` until `[]`), de-duplicate by post id, and
//!    compare every post with the saved copy.
//! 3. **Reconcile**: every saved post missing from the walk is fetched on its own. 404 marks it
//!    gone (record and files kept); 200 means the paging bug hid it and nothing changes.
//!
//! Nothing is written until the scan has finished all three steps; then one transaction records
//! it. A failure or a cancel at any earlier point leaves the library exactly as it was.

use std::collections::HashSet;

use super::fingerprint::fingerprint;
use super::store::{ChangeCounts, LinkRecord, PostWrite, ScanWrite};
use super::Library;
use crate::api::{Client, ScanProgress, PAGE_SIZE};
use crate::control::Control;
use crate::creator::CreatorRef;
use crate::error::{Error, Result};
use crate::links;
use crate::model::Post;

/// An unchanged profile is trusted for this long; after it the whole list is read again, because
/// removals and late file backfills are not proven to move the profile's `updated` time.
pub const FULL_READ_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;

/// Safety net against a server that never returns `[]`.
const MAX_PAGES: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMode {
    /// Gate on the profile's `updated` time, then read the list only if needed.
    Update,
    /// Skip the gate: always walk the whole list and reconcile. The profile is still requested,
    /// once, so its `updated` time is saved for the next update scan.
    Full,
}

/// Where a scan is, for a front end that shows progress. Sent in this order: `Checking` (update
/// scans only), `Reading`, then one `Page` per page, then `Missing` (`done` 0, then once more after
/// each saved post the list did not hold). An update that finds the profile unchanged stops after `Checking`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanStep {
    /// The one profile request that decides whether the list needs reading.
    Checking,
    /// The post list is being read (first scan, full rescan, or a changed or stale profile).
    Reading,
    /// A page of the list arrived.
    Page(ScanProgress),
    /// Saved posts the list did not hold are being asked for one by one.
    Missing { done: usize, total: usize },
}

/// How a post differs from the saved library, for the GUI's New / Updated / Gone from site labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostState {
    New,
    Updated,
    Gone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostChange {
    pub post_id: String,
    pub title: String,
    pub state: PostState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanOutcome {
    pub mode: ScanMode,
    /// The profile was unchanged and under 7 days from its last full read: only the profile was
    /// requested.
    pub unchanged: bool,
    pub new_posts: usize,
    pub updated_posts: usize,
    /// Posts newly found gone from the site in this scan.
    pub gone_posts: usize,
    /// Files first seen in this scan that Pawchive can serve (not `preview_only`, post not
    /// `has_full:false`). Whether they still need downloading is decided by the hash rule when
    /// "Download new only" runs.
    pub new_files: usize,
    /// Saved posts missing from the list that answered 200 on their own: left unchanged.
    pub hidden_live: usize,
    /// Rows the walk returned for a post it had already seen (the paging bug).
    pub duplicates: usize,
    /// Distinct posts the walk found.
    pub posts_seen: usize,
    pub pages: usize,
    /// Requests made, retries not counted.
    pub requests: usize,
    /// New, updated and newly gone posts, in that order, each in list order.
    pub changes: Vec<PostChange>,
}

impl ScanOutcome {
    fn empty(mode: ScanMode) -> Self {
        ScanOutcome {
            mode,
            unchanged: false,
            new_posts: 0,
            updated_posts: 0,
            gone_posts: 0,
            new_files: 0,
            hidden_live: 0,
            duplicates: 0,
            posts_seen: 0,
            pages: 0,
            requests: 0,
            changes: Vec::new(),
        }
    }
}

impl Library {
    /// Scan `creator` and record what changed. See the module docs for the three steps.
    pub async fn scan(
        &self,
        client: &Client,
        creator: &CreatorRef,
        mode: ScanMode,
        control: &Control,
        mut on_step: impl FnMut(ScanStep),
    ) -> Result<ScanOutcome> {
        let saved = self.saved(creator)?;
        let mut out = ScanOutcome::empty(mode);

        // Step 1: the profile. Requested in both modes; only the update scan may stop here.
        if mode == ScanMode::Update {
            on_step(ScanStep::Checking);
        }
        let profile = client.fetch_profile(creator, control).await?;
        out.requests += 1;
        if mode == ScanMode::Update {
            if let Some(rec) = &saved.profile {
                let age = rec.last_full_read_at.map(|t| self.now() - t);
                let fresh = age.is_some_and(|a| (0..FULL_READ_MAX_AGE_SECS).contains(&a));
                let same = profile.updated.is_some() && profile.updated == rec.site_updated;
                if fresh && same {
                    self.touch_scanned(creator)?;
                    out.unchanged = true;
                    return Ok(out);
                }
            }
        }

        // Step 2: walk the whole list.
        on_step(ScanStep::Reading);
        let walked = walk(client, creator, control, &mut on_step, &mut out).await?;
        let walked_ids: HashSet<&str> = walked.iter().map(|p| p.id.as_str()).collect();

        let mut writes = Vec::new();
        let mut updated = Vec::new();
        let mut news = Vec::new();
        for post in &walked {
            let fp = fingerprint(post);
            // A post that came back after being marked gone counts as updated even if nothing else
            // moved (DAN-1661, ui-spec-library section 6).
            let restored = saved.posts.get(&post.id).is_some_and(|s| s.gone);
            let state = match saved.posts.get(&post.id) {
                None => Some(PostState::New),
                Some(s) if restored || s.edited != post.edited || s.fingerprint != fp => Some(PostState::Updated),
                Some(_) => None,
            };
            let change = state.map(|state| PostChange { post_id: post.id.clone(), title: post.title.clone(), state });
            match state {
                Some(PostState::New) => out.new_posts += 1,
                Some(_) => out.updated_posts += 1,
                None => {}
            }
            if state.is_none() {
                continue;
            }
            let files: Vec<_> = post
                .files()
                .into_iter()
                .map(|f| {
                    let available = post.is_downloadable(&f);
                    (f, available)
                })
                .collect();
            out.new_files += files
                .iter()
                .filter(|(f, available)| *available && !saved.files.contains(&(post.id.clone(), f.path.clone())))
                .count();
            let links = links::collect(std::slice::from_ref(post)).iter().map(LinkRecord::from).collect();
            writes.push(PostWrite {
                post: post.clone(),
                changed: state == Some(PostState::Updated),
                fingerprint: fp,
                files,
                links,
            });
            if let Some(c) = change {
                if c.state == PostState::New {
                    news.push(c)
                } else {
                    updated.push(c)
                }
            }
        }

        // Step 3: saved posts the walk did not list. Posts already marked gone are not asked again.
        let mut missing: Vec<&String> = saved
            .posts
            .iter()
            .filter(|(id, s)| !s.gone && !walked_ids.contains(id.as_str()))
            .map(|(id, _)| id)
            .collect();
        missing.sort();
        let total_missing = missing.len();
        let mut gone = Vec::new();
        if total_missing > 0 {
            on_step(ScanStep::Missing { done: 0, total: total_missing });
        }
        for (done, id) in missing.into_iter().enumerate() {
            let found = client.fetch_post(creator, id, control).await?;
            out.requests += 1;
            match found {
                None => gone.push(id.clone()),
                Some(_) => out.hidden_live += 1,
            }
            on_step(ScanStep::Missing { done: done + 1, total: total_missing });
        }
        out.gone_posts = gone.len();

        let mut changes = news;
        changes.extend(updated);
        changes.extend(gone.iter().map(|id| PostChange {
            post_id: id.clone(),
            title: saved.posts.get(id).map(|s| s.title.clone()).unwrap_or_default(),
            state: PostState::Gone,
        }));
        out.changes = changes;
        let found_change = out.new_posts + out.updated_posts + out.gone_posts > 0;
        let change = (saved.profile.is_some() && found_change).then_some(ChangeCounts {
            new_posts: out.new_posts,
            updated_posts: out.updated_posts,
            new_files: out.new_files,
            gone_posts: out.gone_posts,
        });
        self.commit_scan(ScanWrite {
            creator,
            name: profile.name.as_deref(),
            site_updated: profile.updated.as_deref(),
            full_read: true,
            posts: writes,
            gone,
            change,
        })?;
        Ok(out)
    }
}

/// Step 2: every page until `[]`, posts de-duplicated by id (first row wins).
async fn walk(
    client: &Client,
    creator: &CreatorRef,
    control: &Control,
    on_step: &mut impl FnMut(ScanStep),
    out: &mut ScanOutcome,
) -> Result<Vec<Post>> {
    let mut posts: Vec<Post> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut progress = ScanProgress::default();
    let mut prev_first: Option<String> = None;
    for page_no in 0..MAX_PAGES {
        let offset = page_no * PAGE_SIZE;
        let page = client.fetch_page(creator, offset, control).await?;
        out.requests += 1;
        if page.is_empty() {
            out.posts_seen = posts.len();
            return Ok(posts);
        }
        // A server that ignores `o` would hand us the same page forever.
        if prev_first.as_deref() == Some(page[0].id.as_str()) {
            return Err(Error::Parse(format!("page at offset {offset} repeats the previous page; stopping")));
        }
        prev_first = Some(page[0].id.clone());
        out.pages += 1;
        progress.pages += 1;
        for post in page {
            if seen.insert(post.id.clone()) {
                progress.files += post.files().len();
                posts.push(post);
            } else {
                out.duplicates += 1;
            }
        }
        progress.posts = posts.len();
        on_step(ScanStep::Page(progress));
    }
    Err(Error::Parse(format!("gave up after {MAX_PAGES} pages")))
}
