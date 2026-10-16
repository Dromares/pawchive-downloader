//! The Pawchive API client: URL building and paging through a creator's posts.

use serde::Serialize;

use crate::control::Control;
use crate::creator::CreatorRef;
use crate::error::{Error, Result};
use crate::gate::Gate;
use crate::http::{send_with_retry, Http, RetryPolicy};
use crate::model::{parse_posts, FileEntry};

/// Posts per page (`?o=` advances by this much).
pub const PAGE_SIZE: usize = 50;
/// Safety net against a server that never returns `[]`.
const MAX_PAGES: usize = 20_000;

#[derive(Clone, Debug)]
pub struct Hosts {
    pub api: String,
    pub file: String,
    pub thumb: String,
}

impl Default for Hosts {
    fn default() -> Self {
        Hosts {
            api: "https://pawchive.pw".into(),
            file: "https://file.pawchive.pw".into(),
            thumb: "https://img.pawchive.pw".into(),
        }
    }
}

impl Hosts {
    /// `GET {api}/api/v1/{service}/user/{id}?o={offset}`
    pub fn page_url(&self, c: &CreatorRef, offset: usize) -> String {
        format!("{}/api/v1/{}/user/{}?o={offset}", self.api, c.service.as_str(), c.id)
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
            progress.files = files.len();
            on_progress(progress);
        }
        Err(Error::Parse(format!("gave up after {MAX_PAGES} pages")))
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
    }
}
