//! "Download new only": fetch the files the library has not downloaded, record each one as it
//! lands, and never touch a file that is already on disk.

use std::collections::HashMap;
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

    /// Download `files` (a subset of [`Library::pending_files`], say) under
    /// `output/{service}_{creator}/{post folder}/`.
    ///
    /// A file is saved under a name no existing file has, on disk or in the library: when an
    /// edited post replaces `a.png` with new content, the new copy lands as `a (2).png` beside the
    /// old one. The engine runs in no-replace mode, so even a file that appears while a download
    /// is running is never overwritten. Each file is recorded in its own transaction after it is
    /// completely written.
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
        let root = output.join(creator_dir(creator.service.as_str(), &creator.id));
        let taken_in_library = self.saved_paths(creator)?;
        let mut allocators: HashMap<String, NameAllocator> = HashMap::new();
        let mut jobs = Vec::new();
        let mut targets: HashMap<u64, (PendingFile, String)> = HashMap::new();
        for f in files {
            let folder =
                f.folder.clone().unwrap_or_else(|| post_dir(f.post_published.as_deref(), &f.post_id, &f.post_title));
            let wanted = sanitize_component(&f.name, MAX_COMPONENT_BYTES);
            let alloc = allocators.entry(f.post_id.clone()).or_default();
            let dest = loop {
                let name = alloc.allocate(&wanted);
                let candidate = root.join(&folder).join(&name);
                if !taken_in_library.contains(&candidate.to_string_lossy().to_lowercase())
                    && std::fs::symlink_metadata(&candidate).is_err()
                {
                    break candidate;
                }
            };
            let id = jobs.len() as u64;
            let label = format!("{folder}/{}", dest.file_name().unwrap_or_default().to_string_lossy());
            jobs.push(Job { id, url: hosts.full_url(&f.path), dest, expected_size: None, label, ext: None });
            targets.insert(id, (f, folder));
        }

        let recorded = Arc::new(Mutex::new((0usize, Vec::<String>::new())));
        let jobs_by_id: HashMap<u64, PathBuf> = jobs.iter().map(|j| (j.id, j.dest.clone())).collect();
        let (lib, creator_c, rec) = (self.clone(), creator.clone(), recorded.clone());
        let engine = engine.clone().with_no_replace();
        let engine_summary = engine
            .run(jobs, control, move |ev| {
                if let Event::Done { id, .. } = &ev {
                    if let (Some((f, folder)), Some(dest)) = (targets.get(id), jobs_by_id.get(id)) {
                        let r = lib.record_download(&creator_c, &f.post_id, &f.path, dest, folder);
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
