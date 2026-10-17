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
            other => Err(Error::BadUrl(format!("unsupported service \"{other}\" (Pawchive has patreon and fanbox)"))),
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

/// An `embed` that is not an object (never seen, but the API is not ours) is read as absent
/// instead of failing the whole page.
fn lenient_embed<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<RawEmbed>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
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
    /// When Pawchive first indexed the post. Never moves, so it is not a change signal.
    #[serde(default)]
    pub added: Option<String>,
    /// The source platform's edit time as of the latest import. `null` on old imports.
    #[serde(default)]
    pub edited: Option<String>,
    #[serde(default)]
    pub file: Option<RawFile>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub attachments: Vec<RawFile>,
    /// HTML (patreon) or text body. Links to other sites live here (DAN-1609).
    #[serde(default, deserialize_with = "null_as_default")]
    pub content: String,
    /// `{}` on most posts; an embedded link has `url`, `subject` and `description`.
    #[serde(default, deserialize_with = "lenient_embed")]
    pub embed: Option<RawEmbed>,
    /// `false`: Pawchive holds no full-size copy yet, so the files answer 404. Absent in older
    /// samples, which is read as "available".
    #[serde(default)]
    pub has_full: Option<bool>,
    /// `pending` / `scraped`.
    #[serde(default)]
    pub preview_state: Option<String>,
}

/// The `embed` object of a post. Every field is optional (`{}` is the common case).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RawEmbed {
    #[serde(default, deserialize_with = "null_as_default")]
    pub url: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub subject: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub description: String,
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
    /// The size probe got HTTP 404: Pawchive no longer holds the original. Permanent, so such a
    /// file is never queued for download.
    pub missing: bool,
}

impl FileEntry {
    pub fn apply_probe(&mut self, size: Option<u64>, content_type: Option<String>) {
        self.size = size;
        self.content_type = content_type;
    }
}

impl Post {
    /// Pawchive holds a full-size copy of `f`: the post is fully imported and the file is not a
    /// preview. Both flags predicted every file request in the DAN-1656 probe (48 of 48).
    pub fn is_downloadable(&self, f: &FileEntry) -> bool {
        self.has_full != Some(false) && !f.preview_only
    }

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
                missing: false,
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
