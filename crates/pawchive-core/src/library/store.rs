//! The SQLite store: schema, reads, and the two kinds of write (the single end-of-scan commit and
//! the one-row-at-a-time download record).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::DB_FILE;
use crate::creator::CreatorRef;
use crate::error::{Error, Result};
use crate::links::Link;
use crate::model::{FileEntry, Post, Service};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE profiles (
    id INTEGER PRIMARY KEY,
    service TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    name TEXT,
    site_updated TEXT,
    last_scanned_at INTEGER,
    last_full_read_at INTEGER,
    UNIQUE (service, creator_id)
);
CREATE TABLE posts (
    profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    post_id TEXT NOT NULL,
    title TEXT NOT NULL,
    published TEXT,
    added TEXT,
    edited TEXT,
    fingerprint TEXT NOT NULL,
    removed_at INTEGER,
    folder TEXT,
    first_seen_at INTEGER NOT NULL,
    PRIMARY KEY (profile_id, post_id)
);
CREATE TABLE files (
    profile_id INTEGER NOT NULL,
    post_id TEXT NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    hash TEXT NOT NULL,
    available INTEGER NOT NULL,
    downloaded INTEGER NOT NULL DEFAULT 0,
    saved_path TEXT,
    PRIMARY KEY (profile_id, post_id, path),
    FOREIGN KEY (profile_id, post_id) REFERENCES posts(profile_id, post_id) ON DELETE CASCADE
);
CREATE INDEX files_by_hash ON files (profile_id, hash);
CREATE TABLE external_links (
    profile_id INTEGER NOT NULL,
    post_id TEXT NOT NULL,
    url TEXT NOT NULL,
    host TEXT NOT NULL,
    downloadable INTEGER NOT NULL,
    PRIMARY KEY (profile_id, post_id, url),
    FOREIGN KEY (profile_id, post_id) REFERENCES posts(profile_id, post_id) ON DELETE CASCADE
);
";

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// A handle to the library database. Cheap to clone; all clones share one connection.
#[derive(Clone)]
pub struct Library {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
    clock: Clock,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").field("path", &self.path).finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRecord {
    pub creator: CreatorRef,
    pub name: Option<String>,
    /// The profile's `updated` value exactly as the site gave it at the last scan.
    pub site_updated: Option<String>,
    /// Unix seconds. `None` before the first scan finished.
    pub last_scanned_at: Option<i64>,
    /// Unix seconds of the last scan that read the whole post list.
    pub last_full_read_at: Option<i64>,
    pub posts: usize,
    /// Posts marked gone from the site.
    pub gone: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    pub path: String,
    pub name: String,
    /// The file's content hash: the stem of its Pawchive path (the path is the SHA-256 of the
    /// content); the whole path when it has no such stem.
    pub hash: String,
    /// Pawchive holds a full-size copy (not `preview_only`, post not `has_full:false`).
    pub available: bool,
    pub downloaded: bool,
    pub saved_path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRecord {
    pub url: String,
    /// Display name of the host (`Google Drive`, `Mega`, …).
    pub host: String,
    pub downloadable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostRecord {
    pub post_id: String,
    pub title: String,
    pub published: Option<String>,
    pub added: Option<String>,
    pub edited: Option<String>,
    pub fingerprint: String,
    /// Unix seconds the scan found the post gone from the site; its record and files are kept.
    pub removed_at: Option<i64>,
    /// The post's folder name under the creator folder, fixed when its first file is saved.
    pub folder: Option<String>,
    pub files: Vec<FileRecord>,
    pub links: Vec<LinkRecord>,
}

/// What a scan needs from the saved copy of one post.
pub(crate) struct SavedPost {
    pub title: String,
    pub edited: Option<String>,
    pub fingerprint: String,
    pub gone: bool,
}

/// Everything a scan compares against, read once at its start.
pub(crate) struct Saved {
    pub profile: Option<ProfileRecord>,
    pub posts: HashMap<String, SavedPost>,
    /// `(post_id, path)` of every file row.
    pub files: HashSet<(String, String)>,
}

pub(crate) struct PostWrite {
    pub post: Post,
    pub fingerprint: String,
    /// `(file, available)`
    pub files: Vec<(FileEntry, bool)>,
    pub links: Vec<LinkRecord>,
}

pub(crate) struct ScanWrite<'a> {
    pub creator: &'a CreatorRef,
    pub name: Option<&'a str>,
    pub site_updated: Option<&'a str>,
    /// The scan read the whole post list (sets `last_full_read_at`).
    pub full_read: bool,
    pub posts: Vec<PostWrite>,
    pub gone: Vec<String>,
}

/// The content hash of a Pawchive file path: its stem when that is 64 hex digits.
pub fn hash_of_path(path: &str) -> String {
    let leaf = path.rsplit('/').next().unwrap_or(path);
    let stem = leaf.split('.').next().unwrap_or(leaf);
    if stem.len() == 64 && stem.bytes().all(|b| b.is_ascii_hexdigit()) {
        stem.to_ascii_lowercase()
    } else {
        path.to_string()
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Library {
    /// `$XDG_DATA_HOME/pawchive-downloader`, else `~/.local/share/pawchive-downloader`.
    pub fn default_data_dir() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))?;
        Some(base.join("pawchive-downloader"))
    }

    /// Open (creating it if needed) `library.sqlite3` inside `data_dir`. The caller picks the
    /// directory: tests and the dev harness pass a temporary one.
    pub fn open(data_dir: &Path) -> Result<Library> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join(DB_FILE);
        let mut conn = Connection::open(&path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        migrate(&mut conn)?;
        Ok(Library { conn: Arc::new(Mutex::new(conn)), path, clock: Arc::new(now_secs) })
    }

    /// [`Library::open`] on [`Library::default_data_dir`].
    pub fn open_default() -> Result<Library> {
        let dir = Self::default_data_dir().ok_or_else(|| Error::Db("no home directory for the library".into()))?;
        Self::open(&dir)
    }

    /// Replace the clock (unix seconds). Tests use it to age a profile past the 7-day limit.
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Library {
        self.clock = Arc::new(clock);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn now(&self) -> i64 {
        (self.clock)()
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every saved profile, most recently scanned first.
    pub fn profiles(&self) -> Result<Vec<ProfileRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(PROFILE_SELECT_ALL)?;
        let rows = stmt.query_map([], profile_row)?;
        let mut out = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        out.sort_by_key(|a| std::cmp::Reverse(a.last_scanned_at));
        Ok(out)
    }

    pub fn profile(&self, creator: &CreatorRef) -> Result<Option<ProfileRecord>> {
        let conn = self.lock();
        profile_of(&conn, creator)
    }

    /// Forget a profile: its posts, files and links leave the library. Files already downloaded
    /// stay on disk. `false` when it was not saved.
    pub fn remove_profile(&self, creator: &CreatorRef) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM profiles WHERE service = ?1 AND creator_id = ?2",
            params![creator.service.as_str(), creator.id],
        )?;
        Ok(n > 0)
    }

    /// The saved posts of a profile with their files and links, newest first.
    pub fn posts(&self, creator: &CreatorRef) -> Result<Vec<PostRecord>> {
        let conn = self.lock();
        let Some(pid) = profile_id(&conn, creator)? else { return Ok(Vec::new()) };
        let mut files: HashMap<String, Vec<FileRecord>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT post_id, path, name, hash, available, downloaded, saved_path FROM files
                 WHERE profile_id = ?1 ORDER BY rowid",
            )?;
            let rows = stmt.query_map([pid], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    FileRecord {
                        path: r.get(1)?,
                        name: r.get(2)?,
                        hash: r.get(3)?,
                        available: r.get(4)?,
                        downloaded: r.get(5)?,
                        saved_path: r.get::<_, Option<String>>(6)?.map(PathBuf::from),
                    },
                ))
            })?;
            for row in rows {
                let (post, f) = row?;
                files.entry(post).or_default().push(f);
            }
        }
        let mut links: HashMap<String, Vec<LinkRecord>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT post_id, url, host, downloadable FROM external_links WHERE profile_id = ?1 ORDER BY rowid",
            )?;
            let rows = stmt.query_map([pid], |r| {
                Ok((r.get::<_, String>(0)?, LinkRecord { url: r.get(1)?, host: r.get(2)?, downloadable: r.get(3)? }))
            })?;
            for row in rows {
                let (post, l) = row?;
                links.entry(post).or_default().push(l);
            }
        }
        let mut stmt = conn.prepare(
            "SELECT post_id, title, published, added, edited, fingerprint, removed_at, folder FROM posts
             WHERE profile_id = ?1 ORDER BY published DESC, post_id DESC",
        )?;
        let rows = stmt.query_map([pid], |r| {
            Ok(PostRecord {
                post_id: r.get(0)?,
                title: r.get(1)?,
                published: r.get(2)?,
                added: r.get(3)?,
                edited: r.get(4)?,
                fingerprint: r.get(5)?,
                removed_at: r.get(6)?,
                folder: r.get(7)?,
                files: Vec::new(),
                links: Vec::new(),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let mut p = row?;
            p.files = files.remove(&p.post_id).unwrap_or_default();
            p.links = links.remove(&p.post_id).unwrap_or_default();
            out.push(p);
        }
        Ok(out)
    }

    pub(crate) fn saved(&self, creator: &CreatorRef) -> Result<Saved> {
        let conn = self.lock();
        let profile = profile_of(&conn, creator)?;
        let mut saved = Saved { profile, posts: HashMap::new(), files: HashSet::new() };
        let Some(pid) = profile_id(&conn, creator)? else { return Ok(saved) };
        let mut stmt =
            conn.prepare("SELECT post_id, title, edited, fingerprint, removed_at FROM posts WHERE profile_id = ?1")?;
        let rows = stmt.query_map([pid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                SavedPost {
                    title: r.get(1)?,
                    edited: r.get(2)?,
                    fingerprint: r.get(3)?,
                    gone: r.get::<_, Option<i64>>(4)?.is_some(),
                },
            ))
        })?;
        for row in rows {
            let (id, p) = row?;
            saved.posts.insert(id, p);
        }
        let mut stmt = conn.prepare("SELECT post_id, path FROM files WHERE profile_id = ?1")?;
        let rows = stmt.query_map([pid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            saved.files.insert(row?);
        }
        Ok(saved)
    }

    /// A scan found the profile unchanged: only the scan time moves.
    pub(crate) fn touch_scanned(&self, creator: &CreatorRef) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE profiles SET last_scanned_at = ?3 WHERE service = ?1 AND creator_id = ?2",
            params![creator.service.as_str(), creator.id, self.now()],
        )?;
        Ok(())
    }

    /// Write a finished scan: one transaction. Any error rolls the whole scan back.
    pub(crate) fn commit_scan(&self, w: ScanWrite<'_>) -> Result<()> {
        let now = self.now();
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO profiles (service, creator_id, name, site_updated, last_scanned_at, last_full_read_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (service, creator_id) DO UPDATE SET
                name = COALESCE(excluded.name, name),
                site_updated = excluded.site_updated,
                last_scanned_at = excluded.last_scanned_at,
                last_full_read_at = COALESCE(excluded.last_full_read_at, last_full_read_at)",
            params![w.creator.service.as_str(), w.creator.id, w.name, w.site_updated, now, w.full_read.then_some(now)],
        )?;
        let pid: i64 = tx.query_row(
            "SELECT id FROM profiles WHERE service = ?1 AND creator_id = ?2",
            params![w.creator.service.as_str(), w.creator.id],
            |r| r.get(0),
        )?;
        for pw in &w.posts {
            write_post(&tx, pid, pw, now)?;
        }
        for id in &w.gone {
            tx.execute(
                "UPDATE posts SET removed_at = ?3 WHERE profile_id = ?1 AND post_id = ?2 AND removed_at IS NULL",
                params![pid, id, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Files the next "Download new only" fetches: available, not downloaded, in a post that is
    /// still on the site, and whose hash is not downloaded anywhere in the profile. One entry per
    /// hash (the newest post's copy).
    pub(crate) fn pending(&self, creator: &CreatorRef) -> Result<Vec<super::PendingFile>> {
        let conn = self.lock();
        let Some(pid) = profile_id(&conn, creator)? else { return Ok(Vec::new()) };
        let mut stmt = conn.prepare(
            "SELECT f.post_id, p.title, p.published, p.folder, f.name, f.path, f.hash
             FROM files f JOIN posts p ON p.profile_id = f.profile_id AND p.post_id = f.post_id
             WHERE f.profile_id = ?1 AND f.available = 1 AND f.downloaded = 0 AND p.removed_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM files d
                               WHERE d.profile_id = f.profile_id AND d.hash = f.hash AND d.downloaded = 1)
             ORDER BY p.published DESC, f.post_id DESC, f.rowid",
        )?;
        let rows = stmt.query_map([pid], |r| {
            Ok(super::PendingFile {
                post_id: r.get(0)?,
                post_title: r.get(1)?,
                post_published: r.get(2)?,
                folder: r.get(3)?,
                name: r.get(4)?,
                path: r.get(5)?,
                hash: r.get(6)?,
            })
        })?;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for row in rows {
            let f = row?;
            if seen.insert(f.hash.clone()) {
                out.push(f);
            }
        }
        Ok(out)
    }

    /// Every path saved for the profile, lower-cased: names that are taken even if the file was
    /// moved away from disk.
    pub(crate) fn saved_paths(&self, creator: &CreatorRef) -> Result<HashSet<String>> {
        let conn = self.lock();
        let Some(pid) = profile_id(&conn, creator)? else { return Ok(HashSet::new()) };
        let mut stmt = conn.prepare("SELECT saved_path FROM files WHERE profile_id = ?1 AND saved_path IS NOT NULL")?;
        let rows = stmt.query_map([pid], |r| r.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row?.to_lowercase());
        }
        Ok(out)
    }

    /// One finished download, in its own transaction. Called only after the file is in place.
    pub(crate) fn record_download(
        &self,
        creator: &CreatorRef,
        post_id: &str,
        path: &str,
        saved_path: &Path,
        folder: &str,
    ) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let pid = profile_id(&tx, creator)?.ok_or_else(|| Error::Db("profile is not in the library".into()))?;
        let n = tx.execute(
            "UPDATE files SET downloaded = 1, saved_path = ?4 WHERE profile_id = ?1 AND post_id = ?2 AND path = ?3",
            params![pid, post_id, path, saved_path.to_string_lossy()],
        )?;
        if n == 0 {
            return Err(Error::Db(format!("file {path} of post {post_id} is not in the library")));
        }
        tx.execute(
            "UPDATE posts SET folder = COALESCE(folder, ?3) WHERE profile_id = ?1 AND post_id = ?2",
            params![pid, post_id, folder],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    match version {
        0 => {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
            tx.commit()?;
            Ok(())
        }
        SCHEMA_VERSION => Ok(()),
        other => Err(Error::Db(format!(
            "library.sqlite3 has schema version {other}; this version of the app understands {SCHEMA_VERSION}"
        ))),
    }
}

const PROFILE_SELECT_ALL: &str = "SELECT p.service, p.creator_id, p.name, p.site_updated, p.last_scanned_at,
        p.last_full_read_at,
        (SELECT COUNT(*) FROM posts q WHERE q.profile_id = p.id),
        (SELECT COUNT(*) FROM posts q WHERE q.profile_id = p.id AND q.removed_at IS NOT NULL)
     FROM profiles p";

fn profile_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProfileRecord> {
    let service: String = r.get(0)?;
    let service = Service::parse(&service)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    Ok(ProfileRecord {
        creator: CreatorRef { service, id: r.get(1)? },
        name: r.get(2)?,
        site_updated: r.get(3)?,
        last_scanned_at: r.get(4)?,
        last_full_read_at: r.get(5)?,
        posts: r.get::<_, i64>(6)? as usize,
        gone: r.get::<_, i64>(7)? as usize,
    })
}

fn profile_of(conn: &Connection, creator: &CreatorRef) -> Result<Option<ProfileRecord>> {
    let sql = format!("{PROFILE_SELECT_ALL} WHERE p.service = ?1 AND p.creator_id = ?2");
    Ok(conn.query_row(&sql, params![creator.service.as_str(), creator.id], profile_row).optional()?)
}

fn profile_id(conn: &Connection, creator: &CreatorRef) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM profiles WHERE service = ?1 AND creator_id = ?2",
            params![creator.service.as_str(), creator.id],
            |r| r.get(0),
        )
        .optional()?)
}

fn write_post(tx: &Transaction<'_>, pid: i64, pw: &PostWrite, now: i64) -> Result<()> {
    let p = &pw.post;
    tx.execute(
        "INSERT INTO posts (profile_id, post_id, title, published, added, edited, fingerprint, first_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (profile_id, post_id) DO UPDATE SET
            title = excluded.title, published = excluded.published, added = excluded.added,
            edited = excluded.edited, fingerprint = excluded.fingerprint, removed_at = NULL",
        params![pid, p.id, p.title, p.published, p.added, p.edited, pw.fingerprint, now],
    )?;
    // Files: refresh what the post lists, keep every downloaded row (a replaced file leaves the
    // old version recorded), drop only rows never downloaded that the post no longer lists.
    for (f, available) in &pw.files {
        tx.execute(
            "INSERT INTO files (profile_id, post_id, path, name, hash, available) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (profile_id, post_id, path) DO UPDATE SET
                name = excluded.name, hash = excluded.hash, available = excluded.available",
            params![pid, p.id, f.path, f.name, hash_of_path(&f.path), available],
        )?;
    }
    let listed: HashSet<&str> = pw.files.iter().map(|(f, _)| f.path.as_str()).collect();
    let mut stale = Vec::new();
    {
        let mut stmt =
            tx.prepare("SELECT path FROM files WHERE profile_id = ?1 AND post_id = ?2 AND downloaded = 0")?;
        for row in stmt.query_map(params![pid, p.id], |r| r.get::<_, String>(0))? {
            let path = row?;
            if !listed.contains(path.as_str()) {
                stale.push(path);
            }
        }
    }
    for path in stale {
        tx.execute("DELETE FROM files WHERE profile_id = ?1 AND post_id = ?2 AND path = ?3", params![pid, p.id, path])?;
    }
    tx.execute("DELETE FROM external_links WHERE profile_id = ?1 AND post_id = ?2", params![pid, p.id])?;
    for l in &pw.links {
        tx.execute(
            "INSERT OR IGNORE INTO external_links (profile_id, post_id, url, host, downloadable) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![pid, p.id, l.url, l.host, l.downloadable],
        )?;
    }
    Ok(())
}

impl From<&Link> for LinkRecord {
    fn from(l: &Link) -> Self {
        LinkRecord { url: l.url.clone(), host: l.host.name(), downloadable: l.downloadable() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_the_path_stem_when_it_is_a_sha256() {
        let h = "4de5524e41ef3dfb7ea0d862720bf0f10e76744e7d9170d5ac73226078b0be88";
        assert_eq!(hash_of_path(&format!("/4d/e5/{h}.jpg")), h);
        assert_eq!(hash_of_path(&format!("/4d/e5/{}.JPG", h.to_uppercase())), h);
        assert_eq!(hash_of_path("/4d/e5/not-a-hash.jpg"), "/4d/e5/not-a-hash.jpg");
    }

    #[test]
    fn a_database_from_a_newer_version_is_refused_not_modified() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(DB_FILE);
        Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 99;").unwrap();
        let err = Library::open(dir.path()).unwrap_err().to_string();
        assert!(err.contains("schema version 99"), "{err}");
    }

    #[test]
    fn the_default_folder_is_under_the_data_home() {
        // Only the shape: the environment is shared between tests, so it is not changed here.
        if let Some(d) = Library::default_data_dir() {
            assert!(d.ends_with("pawchive-downloader"), "{}", d.display());
        }
    }
}
