//! Wire types for the Pawchive API and the flattened per-file model built from them.

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    Patreon,
    Fanbox,
}

impl Service {
    pub fn as_str(self) -> &'static str {
        match self {
            Service::Patreon => "patreon",
            Service::Fanbox => "fanbox",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "patreon" => Ok(Service::Patreon),
            "fanbox" => Ok(Service::Fanbox),
            other => Err(Error::BadUrl(format!(
                "unsupported service \"{other}\" (Pawchive has patreon and fanbox)"
            ))),
        }
    }
}

/// Accept a JSON string or number (ids have been strings in every sample, but don't depend on it).
fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        S(String),
        N(i64),
    }
    match V::deserialize(d)? {
        V::S(s) => Ok(s),
        V::N(n) => Ok(n.to_string()),
    }
}

fn null_as_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// `file` / `attachments[]` entry. Every field is optional: a post with no main file carries `{}`,
/// fanbox entries have no `preview_only`, and only some zip attachments have `node`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RawFile {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub preview_only: Option<bool>,
    #[serde(default)]
    pub node: Option<i64>,
}

/// One element of a `GET /api/v1/{service}/user/{id}?o=` page (and the post-detail body).
/// Unknown fields (`poll`, `captions`, `has_full`, …) are ignored.
#[derive(Clone, Debug, Deserialize)]
pub struct Post {
    #[serde(deserialize_with = "string_or_number")]
    pub id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub title: String,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub file: Option<RawFile>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub attachments: Vec<RawFile>,
}

/// A file to (possibly) download, flattened out of a post.
#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    pub post_id: String,
    pub post_title: String,
    pub post_published: Option<String>,
    /// Original filename (falls back to the last segment of `path`).
    pub name: String,
    /// Host-relative path, e.g. `/4d/e5/4de5….jpg`.
    pub path: String,
    /// `true` means only a thumbnail exists; the full-size URL returns 404.
    pub preview_only: bool,
    pub node: Option<i64>,
    /// Filled in by the size probe.
    pub size: Option<u64>,
    pub content_type: Option<String>,
}

impl FileEntry {
    pub fn apply_probe(&mut self, size: Option<u64>, content_type: Option<String>) {
        self.size = size;
        self.content_type = content_type;
    }
}

impl Post {
    /// `file` then `attachments`, de-duplicated by `path` within the post (the main file is often
    /// repeated as an attachment). Entries without a path are dropped.
    pub fn files(&self) -> Vec<FileEntry> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for raw in self.file.iter().chain(self.attachments.iter()) {
            let Some(path) = raw.path.as_deref().filter(|p| !p.is_empty()) else {
                continue;
            };
            if !seen.insert(path.to_string()) {
                continue;
            }
            let name = raw
                .name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_string());
            out.push(FileEntry {
                post_id: self.id.clone(),
                post_title: self.title.clone(),
                post_published: self.published.clone(),
                name,
                path: path.to_string(),
                preview_only: raw.preview_only.unwrap_or(false),
                node: raw.node,
                size: None,
                content_type: None,
            });
        }
        out
    }
}

pub fn parse_posts(bytes: &[u8]) -> Result<Vec<Post>> {
    serde_json::from_slice(bytes).map_err(|e| Error::Parse(e.to_string()))
}

pub fn parse_post(bytes: &[u8]) -> Result<Post> {
    serde_json::from_slice(bytes).map_err(|e| Error::Parse(e.to_string()))
}
