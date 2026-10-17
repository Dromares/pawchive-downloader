//! "Download new only": fetch the files the library has not downloaded, record each one as it
//! lands, and never touch a file that is already on disk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::Library;
use crate::api::Hosts;
use crate::control::Control;
use crate::creator::CreatorRef;
use crate::engine::{Engine, Event, Job, Summary};
use crate::error::Result;
use crate::names::{creator_dir, post_dir, sanitize_component, NameAllocator, MAX_COMPONENT_BYTES};

/// A file the library would download now: available on Pawchive, not downloaded, and with a hash
/// that is not downloaded anywhere in the profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingFile {
    pub post_id: String,
    pub post_title: String,
    pub post_published: Option<String>,
    /// The post's folder if an earlier download fixed it.
    pub folder: Option<String>,
    pub name: String,
    pub path: String,
    pub hash: String,
}

/// Which library file a planned job downloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadTarget {
    pub post_id: String,
    pub path: String,
    /// The post folder the job's destination is in.
    pub folder: String,
}

/// Jobs laid out by [`Library::plan_download`]. Job `url`s are empty: the caller knows the file host.
#[derive(Clone, Debug)]
pub struct DownloadPlan {
    pub jobs: Vec<Job>,
    pub targets: HashMap<u64, DownloadTarget>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadSummary {
    /// The engine's tally for this run.
    pub engine: Summary,
    /// Finished files written into the library.
    pub recorded: usize,
    /// A file finished but the library could not record it (the file is on disk).
    pub record_errors: Vec<String>,
}

impl Library {
    /// The files "Download new only" would fetch for `creator` (see [`PendingFile`]).
    pub fn pending_files(&self, creator: &CreatorRef) -> Result<Vec<PendingFile>> {
        self.pending(creator)
    }

    /// Download every pending file of `creator` under `output`.
    pub async fn download_new(
        &self,
        engine: &Engine,
        hosts: &Hosts,
        creator: &CreatorRef,
        output: &Path,
        control: Control,
        emit: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<DownloadSummary> {
        let files = self.pending_files(creator)?;
        self.download_files(engine, hosts, creator, output, files, control, emit).await
    }

    /// The files a download starts from: every file of the profile `keys` names (`(post_id, path)`),
    /// or with `only_new` just those among them that [`Library::pending_files`] would fetch.
    pub fn download_candidates(
        &self,
        creator: &CreatorRef,
        keys: &HashSet<(String, String)>,
        only_new: bool,
    ) -> Result<Vec<PendingFile>> {
        let all = if only_new { self.pending(creator)? } else { self.available_files(creator)? };
        Ok(all.into_iter().filter(|f| keys.contains(&(f.post_id.clone(), f.path.clone()))).collect())
    }

    /// Lay out the downloads of `files` under `output/{service}_{creator}/{post folder}/`.
    ///
    /// A file the library saved before goes back to the same place (a download of everything fills
    /// a gap there); any other is saved under a name no existing file has, on disk or in the
    /// library: when an edited post replaces `a.png` with new content, the new copy lands as
    /// `a (2).png` beside the old one.
    pub fn plan_download(&self, creator: &CreatorRef, output: &Path, files: Vec<PendingFile>) -> Result<DownloadPlan> {
        let root = output.join(creator_dir(creator.service.as_str(), &creator.id));
        let taken_in_library = self.saved_paths(creator)?;
        let before = self.saved_locations(creator)?;
        let mut allocators: HashMap<String, NameAllocator> = HashMap::new();
        let mut jobs = Vec::new();
        let mut targets: HashMap<u64, DownloadTarget> = HashMap::new();
        for f in files {
            let folder =
                f.folder.clone().unwrap_or_else(|| post_dir(f.post_published.as_deref(), &f.post_id, &f.post_title));
            let dest = match before.get(&(f.post_id.clone(), f.path.clone())) {
                Some(saved) => saved.clone(),
                None => {
                    let wanted = sanitize_component(&f.name, MAX_COMPONENT_BYTES);
                    let alloc = allocators.entry(f.post_id.clone()).or_default();
                    loop {
                        let name = alloc.allocate(&wanted);
                        let candidate = root.join(&folder).join(&name);
                        if !taken_in_library.contains(&candidate.to_string_lossy().to_lowercase())
                            && std::fs::symlink_metadata(&candidate).is_err()
                        {
                            break candidate;
                        }
                    }
                }
            };
            let id = jobs.len() as u64;
            let label = format!("{folder}/{}", dest.file_name().unwrap_or_default().to_string_lossy());
            jobs.push(Job { id, url: String::new(), dest, expected_size: None, label, ext: None });
            targets.insert(id, DownloadTarget { post_id: f.post_id, path: f.path, folder });
        }
        Ok(DownloadPlan { jobs, targets })
    }

    /// Record one finished download. Called only after the file is completely in place.
    pub fn record_done(&self, creator: &CreatorRef, target: &DownloadTarget, dest: &Path) -> Result<()> {
        self.record_download(creator, &target.post_id, &target.path, dest, &target.folder)
    }

    /// Download `files` (a subset of [`Library::pending_files`], say) as [`Library::plan_download`]
    /// lays them out. The engine runs in no-replace mode, so even a file that appears while a
    /// download is running is never overwritten. Each file is recorded in its own transaction after
    /// it is completely written.
    #[allow(clippy::too_many_arguments)]
    pub async fn download_files(
        &self,
        engine: &Engine,
        hosts: &Hosts,
        creator: &CreatorRef,
        output: &Path,
        files: Vec<PendingFile>,
        control: Control,
        emit: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<DownloadSummary> {
        let mut plan = self.plan_download(creator, output, files)?;
        let urls: HashMap<u64, String> = plan.targets.iter().map(|(id, t)| (*id, hosts.full_url(&t.path))).collect();
        for j in &mut plan.jobs {
            j.url = urls.get(&j.id).cloned().unwrap_or_default();
        }
        let DownloadPlan { jobs, targets } = plan;
        let recorded = Arc::new(Mutex::new((0usize, Vec::<String>::new())));
        let jobs_by_id: HashMap<u64, PathBuf> = jobs.iter().map(|j| (j.id, j.dest.clone())).collect();
        let (lib, creator_c, rec) = (self.clone(), creator.clone(), recorded.clone());
        let engine = engine.clone().with_no_replace();
        let engine_summary = engine
            .run(jobs, control, move |ev| {
                if let Event::Done { id, .. } = &ev {
                    if let (Some(t), Some(dest)) = (targets.get(id), jobs_by_id.get(id)) {
                        let r = lib.record_done(&creator_c, t, dest);
                        let mut g = rec.lock().unwrap_or_else(|e| e.into_inner());
                        match r {
                            Ok(()) => g.0 += 1,
                            Err(e) => g.1.push(format!("{}: {e}", dest.display())),
                        }
                    }
                }
                emit(ev);
            })
            .await;
        let (n, errors) = std::mem::take(&mut *recorded.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(DownloadSummary { engine: engine_summary, recorded: n, record_errors: errors })
    }
}
