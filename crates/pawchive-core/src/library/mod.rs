//! The saved library (DAN-1655): one SQLite database holding every profile the user has scanned,
//! its posts, files and external links, plus the incremental scan and "download new only" logic
//! built on it.
//!
//! * [`Library`] is the store. Its data directory is a parameter, so tests and the dev harness
//!   never touch the real one ([`Library::default_data_dir`]).
//! * [`scan`] reads a profile and records what changed (one transaction at the end).
//! * [`download`] fetches the files that are not downloaded yet and records each one as it lands.

mod download;
mod fingerprint;
mod scan;
mod store;
mod view;

pub use download::{DownloadPlan, DownloadSummary, DownloadTarget, PendingFile};
pub use fingerprint::fingerprint;
pub use scan::{PostChange, PostState, ScanMode, ScanOutcome, ScanStep, FULL_READ_MAX_AGE_SECS};
pub use store::{ChangeSummary, FileRecord, Library, LinkRecord, PostLabel, PostRecord, ProfileRecord};
pub use view::{LoadedProfile, PostRow, SavedFile};

/// File name of the database inside the data directory.
pub const DB_FILE: &str = "library.sqlite3";
