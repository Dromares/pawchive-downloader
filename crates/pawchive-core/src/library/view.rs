//! The saved profile as a front end shows it: the post list with labels, and the files and links in
//! the shapes the scan screen already understands. Read-only, no network.

use std::collections::HashSet;
use std::path::PathBuf;

use super::store::{PostLabel, PostRecord, ProfileRecord};
use super::Library;
use crate::creator::CreatorRef;
use crate::error::Result;
use crate::links::{self, Link, PostRef};
use crate::model::FileEntry;

/// One file of a saved profile, as the scan screen's [`FileEntry`] plus what the library knows.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedFile {
    pub entry: FileEntry,
    pub downloaded: bool,
    pub saved_path: Option<PathBuf>,
    /// "Download new only" would fetch it: available, not downloaded, in a post still on the site,
    /// and its content is not downloaded anywhere in the profile.
    pub pending: bool,
}

/// One row of the Posts tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostRow {
    pub post_id: String,
    pub title: String,
    pub published: Option<String>,
    pub label: Option<PostLabel>,
    /// Files the post lists; for a gone post, the files saved from it.
    pub files: usize,
}

/// A saved profile read back for display.
#[derive(Clone, Debug)]
pub struct LoadedProfile {
    pub record: ProfileRecord,
    /// Newest first, the site's order.
    pub posts: Vec<PostRow>,
    pub files: Vec<SavedFile>,
    pub links: Vec<Link>,
}

impl LoadedProfile {
    pub fn count_label(&self, label: PostLabel) -> usize {
        self.posts.iter().filter(|p| p.label == Some(label)).count()
    }
}

impl Library {
    /// Read `creator` back from the library. `None` when it is not saved.
    ///
    /// A file Pawchive holds no full-size copy of (`available` false), and every file of a post that
    /// is gone from the site, comes back as `missing`: listed as "Not on Pawchive", never offered.
    pub fn load(&self, creator: &CreatorRef) -> Result<Option<LoadedProfile>> {
        let Some(record) = self.profile(creator)? else { return Ok(None) };
        let records = self.posts(creator)?;
        let pending: HashSet<(String, String)> =
            self.pending(creator)?.into_iter().map(|f| (f.post_id, f.path)).collect();
        let last_change_at = record.last_change.map(|c| c.at);
        let mut posts = Vec::with_capacity(records.len());
        let mut files = Vec::new();
        let mut found: Vec<(PostRef, Vec<String>)> = Vec::new();
        for p in &records {
            let gone = p.removed_at.is_some();
            posts.push(PostRow {
                post_id: p.post_id.clone(),
                title: p.title.clone(),
                published: p.published.clone(),
                label: p.label(last_change_at),
                files: if gone { p.files.iter().filter(|f| f.downloaded).count() } else { p.files.len() },
            });
            for f in &p.files {
                files.push(SavedFile {
                    entry: file_entry(p, f, gone),
                    downloaded: f.downloaded,
                    saved_path: f.saved_path.clone(),
                    pending: pending.contains(&(p.post_id.clone(), f.path.clone())),
                });
            }
            if !p.links.is_empty() {
                found.push((
                    PostRef { id: p.post_id.clone(), title: p.title.clone(), published: p.published.clone() },
                    p.links.iter().map(|l| l.url.clone()).collect(),
                ));
            }
        }
        Ok(Some(LoadedProfile { record, posts, files, links: links::from_urls(&found) }))
    }
}

fn file_entry(p: &PostRecord, f: &super::FileRecord, post_gone: bool) -> FileEntry {
    FileEntry {
        post_id: p.post_id.clone(),
        post_title: p.title.clone(),
        post_published: p.published.clone(),
        name: f.name.clone(),
        path: f.path.clone(),
        preview_only: false,
        node: None,
        size: None,
        content_type: None,
        missing: post_gone || !f.available,
    }
}
