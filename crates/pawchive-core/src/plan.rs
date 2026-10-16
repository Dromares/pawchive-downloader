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

/// A file whose original is gone from the server (`missing`) but which is an image, so the
/// thumbnail host very likely still has it. Decided by extension alone (no custom extensions, no
/// probe): thumbnails are not probed at scan time.
pub fn missing_with_thumbnail(f: &FileEntry) -> bool {
    f.missing && classify(&f.name, &f.path, &CustomExts::default(), None) == Category::Image
}

/// The file can only be had as a thumbnail: the API says so, or the original answered 404 and it
/// is an image.
pub fn thumbnail_only(f: &FileEntry) -> bool {
    f.preview_only || missing_with_thumbnail(f)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileSummary {
    /// Downloadable (full-size) files per category.
    pub per_category: BTreeMap<Category, Stat>,
    /// Thumbnail-only files, kept out of the category rows: API `preview_only` files plus missing
    /// images (`preview_missing` of them).
    pub preview: Stat,
    /// How many of `preview` are images whose original answered 404 (not API `preview_only`).
    pub preview_missing: usize,
    /// All files whose original the server answered 404 for when sized. They are in no category
    /// row; the images among them are also counted in `preview`.
    pub missing: usize,
}

pub fn summarize(files: &[FileEntry], custom: &CustomExts) -> FileSummary {
    let mut s = FileSummary::default();
    for f in files {
        if f.missing {
            s.missing += 1;
        }
        if missing_with_thumbnail(f) {
            s.preview_missing += 1;
            s.preview.add(f);
        } else if f.missing {
            // report-only: no thumbnail exists for it
        } else if f.preview_only {
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
        let thumb = thumbnail_only(f);
        let mut name = sanitize_component(&f.name, MAX_COMPONENT_BYTES);
        if thumb {
            name = sanitize_component(&thumb_name(&name), MAX_COMPONENT_BYTES);
        }
        let name = alloc.allocate(&name);
        if f.missing && !thumb {
            continue; // the server already said 404 and no thumbnail exists: a download could only fail
        }
        let selected = if thumb { sel.include_preview } else { sel.categories.contains(&category_of(f, custom)) };
        if !selected {
            continue;
        }
        let dest = root.join(&*dir).join(&name);
        jobs.push(Job {
            id: jobs.len() as u64,
            url: if thumb { hosts.thumb_url(&f.path) } else { hosts.full_url(&f.path) },
            dest,
            expected_size: if thumb { None } else { f.size },
            label: format!("{dir}/{name}"),
        });
    }
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creator::CreatorRef;
    use crate::model::Service;

    fn gone(name: &str) -> FileEntry {
        FileEntry {
            post_id: "1".into(),
            post_title: "t".into(),
            post_published: None,
            name: name.into(),
            path: format!("/ab/{name}"),
            preview_only: false,
            node: None,
            size: None,
            content_type: None,
            missing: true,
        }
    }

    /// Missing images become `.thumb` jobs on the thumbnail host, only when Preview only is ticked;
    /// missing non-images never become jobs (no thumbnail exists for them).
    #[test]
    fn missing_images_plan_as_thumbnails_and_other_missing_files_never_plan() {
        let creator = CreatorRef { service: Service::Fanbox, id: "9".into() };
        let files = vec![gone("a.jpg"), gone("b.zip"), gone("c.txt"), gone("d.PNG")];
        let plan = |include_preview| {
            let sel = Selection { categories: Category::ALL.into_iter().collect(), include_preview };
            plan_downloads(&creator, &files, &sel, &CustomExts::default(), Path::new("/o"), &Hosts::default())
        };
        assert!(plan(false).is_empty());
        let jobs = plan(true);
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|j| j.label.contains(".thumb.") && j.expected_size.is_none()));
        assert!(jobs.iter().all(|j| j.url.contains("/thumbnail/")), "{:?}", jobs[0].url);
        let s = summarize(&files, &CustomExts::default());
        assert_eq!((s.missing, s.preview_missing, s.preview.count), (4, 2, 2));
        assert!(s.per_category.is_empty());
    }
}
