//! The Pawchive API client: URL building and paging through a creator's posts.

use serde::Serialize;

use crate::control::Control;
use crate::creator::CreatorRef;
use crate::error::{Error, Result};
use crate::gate::Gate;
use crate::http::{send_with_retry, Http, RetryPolicy};
use crate::links::{self, Link, PostRef};
use crate::model::{parse_post, parse_posts, FileEntry, Post};

/// Posts per page (`?o=` advances by this much).
pub const PAGE_SIZE: usize = 50;
/// Safety net against a server that never returns `[]`.
const MAX_PAGES: usize = 20_000;

#[derive(Clone, Debug)]
pub struct Hosts {
    pub api: String,
    pub file: String,
    pub thumb: String,
    /// Tests and the dev harness only: send every request to Google Drive and Mediafire to this
    /// base URL instead (scheme and host replaced, path and query kept).
    pub external: Option<String>,
}

impl Default for Hosts {
    fn default() -> Self {
        Hosts {
            api: "https://pawchive.pw".into(),
            file: "https://file.pawchive.pw".into(),
            thumb: "https://img.pawchive.pw".into(),
            external: None,
        }
    }
}

impl Hosts {
    /// `GET {api}/api/v1/{service}/user/{id}?o={offset}`
    pub fn page_url(&self, c: &CreatorRef, offset: usize) -> String {
        format!("{}/api/v1/{}/user/{}?o={offset}", self.api, c.service.as_str(), c.id)
    }

    /// `GET {api}/api/v1/{service}/user/{id}/profile`: name and the `updated` time.
    pub fn profile_url(&self, c: &CreatorRef) -> String {
        format!("{}/api/v1/{}/user/{}/profile", self.api, c.service.as_str(), c.id)
    }

    /// `GET {api}/api/v1/{service}/user/{id}/post/{post_id}`: one post, flat object.
    pub fn post_url(&self, c: &CreatorRef, post_id: &str) -> String {
        format!("{}/api/v1/{}/user/{}/post/{post_id}", self.api, c.service.as_str(), c.id)
    }

    /// The URL to really request for an external `url`: unchanged, or re-based on `external`.
    pub fn external_url(&self, url: &str) -> String {
        let Some(base) = &self.external else { return url.to_string() };
        match url::Url::parse(url) {
            Ok(u) => {
                let mut out = format!("{}{}", base.trim_end_matches('/'), u.path());
                if let Some(q) = u.query() {
                    out.push('?');
                    out.push_str(q);
                }
                out
            }
            Err(_) => url.to_string(),
        }
    }

    /// Full-size file: `{file}/data{path}`
    pub fn full_url(&self, path: &str) -> String {
        format!("{}/data{}", self.file, with_slash(path))
    }

    /// Thumbnail: `{thumb}/thumbnail/data{path}`
    pub fn thumb_url(&self, path: &str) -> String {
        format!("{}/thumbnail/data{}", self.thumb, with_slash(path))
    }
}

fn with_slash(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ScanProgress {
    pub pages: usize,
    pub posts: usize,
    pub files: usize,
}

#[derive(Clone, Debug)]
pub struct ScanResult {
    pub creator: CreatorRef,
    pub pages: usize,
    pub posts: usize,
    pub files: Vec<FileEntry>,
    /// External links found in the posts' text, de-duplicated, in display order.
    pub links: Vec<Link>,
    /// Set for a single-post scan.
    pub post: Option<PostRef>,
}

/// The two fields of `…/profile` the library uses. Everything else is ignored.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct ProfileInfo {
    #[serde(default)]
    pub name: Option<String>,
    /// When Pawchive last updated this creator. Moves when a post is added or re-imported.
    #[serde(default)]
    pub updated: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Client {
    pub http: Http,
    pub gate: Gate,
    pub hosts: Hosts,
    pub policy: RetryPolicy,
}

impl Client {
    /// Page through `?o=0,50,100,…` until the API answers `[]`, flattening every post's files.
    pub async fn scan(
        &self,
        creator: &CreatorRef,
        control: &Control,
        mut on_progress: impl FnMut(ScanProgress),
    ) -> Result<ScanResult> {
        let mut files = Vec::new();
        let mut all_posts: Vec<Post> = Vec::new();
        let mut progress = ScanProgress::default();
        let mut prev_first: Option<String> = None;
        for page_no in 0..MAX_PAGES {
            let offset = page_no * PAGE_SIZE;
            let url = self.hosts.page_url(creator, offset);
            let resp = send_with_retry(&self.gate, control, &self.policy, || {
                self.http.get(&url).header(reqwest::header::ACCEPT, "application/json")
            })
            .await?;
            let body = resp.bytes().await.map_err(Error::from_reqwest)?;
            let posts = parse_posts(&body)?;
            if posts.is_empty() {
                return Ok(ScanResult {
                    creator: creator.clone(),
                    pages: progress.pages,
                    posts: progress.posts,
                    files,
                    links: links::collect(&all_posts),
                    post: None,
                });
            }
            // A server that ignores `o` would hand us the same page forever.
            if prev_first.as_deref() == Some(posts[0].id.as_str()) {
                return Err(Error::Parse(format!("page at offset {offset} repeats the previous page; stopping")));
            }
            prev_first = Some(posts[0].id.clone());
            progress.pages += 1;
            progress.posts += posts.len();
            for p in &posts {
                files.extend(p.files());
            }
            all_posts.extend(posts);
            progress.files = files.len();
            on_progress(progress);
        }
        Err(Error::Parse(format!("gave up after {MAX_PAGES} pages")))
    }

    /// One request for the profile (name and `updated`). 404 is [`Error::NotFound`].
    pub async fn fetch_profile(&self, creator: &CreatorRef, control: &Control) -> Result<ProfileInfo> {
        let url = self.hosts.profile_url(creator);
        let resp = send_with_retry(&self.gate, control, &self.policy, || {
            self.http.get(&url).header(reqwest::header::ACCEPT, "application/json")
        })
        .await?;
        let body = resp.bytes().await.map_err(Error::from_reqwest)?;
        serde_json::from_slice(&body).map_err(|e| Error::Parse(e.to_string()))
    }

    /// One page of the post list (`?o=offset`). An empty page ends the list.
    pub async fn fetch_page(&self, creator: &CreatorRef, offset: usize, control: &Control) -> Result<Vec<Post>> {
        let url = self.hosts.page_url(creator, offset);
        let resp = send_with_retry(&self.gate, control, &self.policy, || {
            self.http.get(&url).header(reqwest::header::ACCEPT, "application/json")
        })
        .await?;
        parse_posts(&resp.bytes().await.map_err(Error::from_reqwest)?)
    }

    /// One post by id; `None` when the site answers 404 (the post is gone).
    pub async fn fetch_post(&self, creator: &CreatorRef, post_id: &str, control: &Control) -> Result<Option<Post>> {
        let url = self.hosts.post_url(creator, post_id);
        let resp = send_with_retry(&self.gate, control, &self.policy, || {
            self.http.get(&url).header(reqwest::header::ACCEPT, "application/json")
        })
        .await;
        match resp {
            Ok(resp) => parse_post(&resp.bytes().await.map_err(Error::from_reqwest)?).map(Some),
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// One request for a single post (`/post/{id}` links): its files and the links in its text.
    pub async fn scan_post(&self, creator: &CreatorRef, post_id: &str, control: &Control) -> Result<ScanResult> {
        let url = self.hosts.post_url(creator, post_id);
        let resp = send_with_retry(&self.gate, control, &self.policy, || {
            self.http.get(&url).header(reqwest::header::ACCEPT, "application/json")
        })
        .await?;
        let body = resp.bytes().await.map_err(Error::from_reqwest)?;
        let post = parse_post(&body)?;
        let files = post.files();
        let pref = PostRef { id: post.id.clone(), title: post.title.clone(), published: post.published.clone() };
        Ok(ScanResult {
            creator: creator.clone(),
            pages: 1,
            posts: 1,
            links: links::collect(std::slice::from_ref(&post)),
            files,
            post: Some(pref),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Service;

    #[test]
    fn urls_follow_the_documented_shapes() {
        let h = Hosts::default();
        let c = CreatorRef { service: Service::Patreon, id: "30500811".into() };
        assert_eq!(h.page_url(&c, 50), "https://pawchive.pw/api/v1/patreon/user/30500811?o=50");
        assert_eq!(h.full_url("/4d/e5/x.jpg"), "https://file.pawchive.pw/data/4d/e5/x.jpg");
        assert_eq!(h.thumb_url("/4d/e5/x.jpg"), "https://img.pawchive.pw/thumbnail/data/4d/e5/x.jpg");
        assert_eq!(h.full_url("4d/x"), "https://file.pawchive.pw/data/4d/x");
        assert_eq!(h.post_url(&c, "172435214"), "https://pawchive.pw/api/v1/patreon/user/30500811/post/172435214");
        assert_eq!(h.external_url("https://drive.google.com/uc?id=1&x=2"), "https://drive.google.com/uc?id=1&x=2");
        let t = Hosts { external: Some("http://127.0.0.1:9/".into()), ..Hosts::default() };
        assert_eq!(t.external_url("https://drive.google.com/uc?id=1&x=2#f"), "http://127.0.0.1:9/uc?id=1&x=2");
    }
}
