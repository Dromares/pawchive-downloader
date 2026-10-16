//! Turns the scanned file list plus the user's selection into download jobs, and summarises the
//! list for the type-selection screen.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::api::Hosts;
use crate::classify::{classify, Category, CustomExts};
use crate::creator::CreatorRef;
use crate::engine::Job;
use crate::model::FileEntry;
use crate::names::{creator_dir, post_dir, sanitize_component, thumb_name, NameAllocator, MAX_COMPONENT_BYTES};

/// Category of a (non-preview) file, using the probed Content-Type when we have one.
pub fn category_of(f: &FileEntry, custom: &CustomExts) -> Category {
    classify(&f.name, &f.path, custom, f.content_type.as_deref())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub count: usize,
    /// Sum of the sizes we know.
    pub bytes: u64,
    /// Files whose size is not known (yet).
    pub unsized_files: usize,
}

impl Stat {
    fn add(&mut self, f: &FileEntry) {
        self.count += 1;
        match f.size {
            Some(s) => self.bytes += s,
            None => self.unsized_files += 1,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileSummary {
    /// Downloadable (full-size) files per category.
    pub per_category: BTreeMap<Category, Stat>,
    /// Preview-only files (thumbnail only), kept out of the category rows.
    pub preview: Stat,
}

pub fn summarize(files: &[FileEntry], custom: &CustomExts) -> FileSummary {
    let mut s = FileSummary::default();
    for f in files {
        if f.preview_only {
            s.preview.add(f);
        } else {
            s.per_category.entry(category_of(f, custom)).or_default().add(f);
        }
    }
    s
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub categories: HashSet<Category>,
    /// Also download preview-only files as thumbnails (`name.thumb.ext`).
    pub include_preview: bool,
}

/// Build jobs for the selected files. Names are allocated over *all* of a post's files, selected
/// or not, so a file keeps the same name (and skip-existing keeps working) when the selection
/// changes between runs.
pub fn plan_downloads(
    creator: &CreatorRef,
    files: &[FileEntry],
    sel: &Selection,
    custom: &CustomExts,
    output: &Path,
    hosts: &Hosts,
) -> Vec<Job> {
    let root = output.join(creator_dir(creator.service.as_str(), &creator.id));
    let mut per_post: HashMap<&str, (String, NameAllocator)> = HashMap::new();
    let mut jobs = Vec::new();
    for f in files {
        let (dir, alloc) = per_post.entry(f.post_id.as_str()).or_insert_with(|| {
            (post_dir(f.post_published.as_deref(), &f.post_id, &f.post_title), NameAllocator::new())
        });
        let mut name = sanitize_component(&f.name, MAX_COMPONENT_BYTES);
        if f.preview_only {
            name = sanitize_component(&thumb_name(&name), MAX_COMPONENT_BYTES);
        }
        let name = alloc.allocate(&name);
        let selected = if f.preview_only {
            sel.include_preview
        } else {
            sel.categories.contains(&category_of(f, custom))
        };
        if !selected {
            continue;
        }
        let dest = root.join(&*dir).join(&name);
        jobs.push(Job {
            id: jobs.len() as u64,
            url: if f.preview_only { hosts.thumb_url(&f.path) } else { hosts.full_url(&f.path) },
            dest,
            expected_size: if f.preview_only { None } else { f.size },
            label: format!("{dir}/{name}"),
        });
    }
    jobs
}
