//! Downloading from other sites (DAN-1609): Google Drive files and folders and Mediafire files.
//!
//! These hosts get their own HTTP client (plain `pawchive-downloader/<version>` user agent, never the
//! contact string, never cookies) and their own request clock each (`HostGates`), so a throttled
//! Drive never slows Pawchive or Mediafire down.

pub mod drive;
pub mod mediafire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION};
use reqwest::{RequestBuilder, Response, StatusCode};

use crate::api::Hosts;
use crate::control::Control;
use crate::gate::Gate;
use crate::http::{retry_after, Http, RetryPolicy};
use crate::links::{Link, LinkKind};
use crate::probe::total_from_content_range;

/// The two external sites we can download from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExtHost {
    Drive,
    Mediafire,
}

impl ExtHost {
    pub fn name(self) -> &'static str {
        match self {
            ExtHost::Drive => "Google Drive",
            ExtHost::Mediafire => "Mediafire",
        }
    }
}

/// One request clock per external host, shared by checks and downloads.
#[derive(Clone, Debug)]
pub struct HostGates {
    interval: Duration,
    map: Arc<Mutex<HashMap<ExtHost, Gate>>>,
}

impl HostGates {
    pub fn new(interval: Duration) -> Self {
        HostGates { interval, map: Arc::default() }
    }

    pub fn gate(&self, host: ExtHost) -> Gate {
        self.map.lock().unwrap().entry(host).or_insert_with(|| Gate::new(self.interval)).clone()
    }
}

/// Why nothing can be downloaded from a link. Permanent: never retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeadReason {
    Removed,
    Private,
    Password,
    Invalid,
    Empty,
}

impl DeadReason {
    /// Second line of a dead row (at most 38 characters).
    pub fn text(self, host: ExtHost) -> String {
        match (self, host) {
            (DeadReason::Removed, h) => format!("File removed on {}", h.name()),
            (DeadReason::Private, _) => "Not shared publicly".into(),
            (DeadReason::Password, _) => "Password needed".into(),
            (DeadReason::Invalid, _) => "Link is invalid".into(),
            (DeadReason::Empty, _) => "Empty folder".into(),
        }
    }

    /// The machine-readable `note` of the CSV export.
    pub fn code(self) -> &'static str {
        match self {
            DeadReason::Removed => "removed",
            DeadReason::Private => "private",
            DeadReason::Password => "password",
            DeadReason::Invalid => "invalid",
            DeadReason::Empty => "empty",
        }
    }
}

/// One file inside a Drive folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    pub id: String,
    /// Folder names from the shared folder down to this file (empty for a file at the top).
    pub dirs: Vec<String>,
    pub name: String,
    pub size: Option<u64>,
    pub mime: Option<String>,
}

impl Child {
    /// `Lineart/1.jpg`
    pub fn path(&self) -> String {
        let mut p = self.dirs.join("/");
        if !p.is_empty() {
            p.push('/');
        }
        p.push_str(&self.name);
        p
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedKind {
    File,
    Folder {
        children: Vec<Child>,
        /// The listing was not confirmed complete (see `drive::PAGE_LIMIT`): there may be more files.
        partial: bool,
    },
}

/// What checking a link found out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The file's real name, or the folder's name.
    pub name: String,
    /// File size; for a folder the sum of the known child sizes.
    pub size: Option<u64>,
    pub content_type: Option<String>,
    pub kind: ResolvedKind,
}

impl Resolved {
    /// Files this link yields.
    pub fn file_count(&self) -> usize {
        match &self.kind {
            ResolvedKind::File => 1,
            ResolvedKind::Folder { children, .. } => children.len(),
        }
    }

    pub fn partial(&self) -> bool {
        matches!(self.kind, ResolvedKind::Folder { partial: true, .. })
    }

    /// Whether every file's size is known.
    pub fn size_complete(&self) -> bool {
        match &self.kind {
            ResolvedKind::File => self.size.is_some(),
            ResolvedKind::Folder { children, .. } => children.iter().all(|c| c.size.is_some()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// Permanent: the link has nothing to download.
    Dead(DeadReason),
    /// Worth trying again later. `quota` marks a download-limit / throttling answer.
    Transient { message: String, quota: bool },
    Cancelled,
}

impl ResolveError {
    pub(crate) fn transient(message: impl Into<String>) -> Self {
        ResolveError::Transient { message: message.into(), quota: false }
    }
}

/// Something the UI should know about while a request is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// The host answered 403/429: its clock stops for this long. Other hosts carry on.
    Cooldown { host: ExtHost, secs: u64 },
}

pub type Notify<'a> = &'a (dyn Fn(Notice) + Send + Sync);

/// A request that could not be completed even after retrying.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    Cancelled,
    /// 403/429 until the retries ran out.
    Throttled(String),
    Network(String),
}

impl From<FetchError> for ResolveError {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Cancelled => ResolveError::Cancelled,
            FetchError::Throttled(m) => ResolveError::Transient { message: m, quota: true },
            FetchError::Network(m) => ResolveError::transient(m),
        }
    }
}

/// Everything needed to talk to the external hosts.
#[derive(Clone, Debug)]
pub struct ExtClient {
    pub http: Http,
    pub gates: HostGates,
    pub hosts: Hosts,
    pub policy: RetryPolicy,
}

impl ExtClient {
    pub fn new(hosts: Hosts, interval: Duration, policy: RetryPolicy) -> crate::Result<Self> {
        Ok(ExtClient { http: Http::external()?, gates: HostGates::new(interval), hosts, policy })
    }

    /// `GET url` through the host's own clock. Redirects are followed by the client (except to a
    /// sign-in page or Mediafire's error page, which come back as the 3xx so callers can read them).
    /// 403 and 429 start a cool-down on this host only and are retried; 5xx and network errors back
    /// off. Every other status is handed back for the caller to judge.
    pub async fn get(
        &self,
        host: ExtHost,
        control: &Control,
        notify: Notify<'_>,
        url: &str,
        extra: &[(&str, String)],
    ) -> Result<Response, FetchError> {
        let url = self.hosts.external_url(url);
        let gate = self.gates.gate(host);
        let mut attempt = 0u32;
        loop {
            if !gate.acquire(control).await {
                return Err(FetchError::Cancelled);
            }
            attempt += 1;
            let mut rb: RequestBuilder = self.http.get(&url);
            for (k, v) in extra {
                rb = rb.header(*k, v.as_str());
            }
            let (reason, wait, throttled) = match rb.send().await {
                Ok(resp) => match resp.status() {
                    StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
                        let pause = self.policy.forbidden_pause.max(retry_after(resp.headers()).unwrap_or_default());
                        gate.pause_for(pause);
                        notify(Notice::Cooldown { host, secs: pause.as_secs() });
                        (format!("HTTP {}", resp.status().as_u16()), Duration::ZERO, true)
                    }
                    s if s.is_server_error() => {
                        let ra = retry_after(resp.headers());
                        (format!("HTTP {}", s.as_u16()), self.policy.backoff(attempt, ra), false)
                    }
                    _ => return Ok(resp),
                },
                Err(e) => (crate::Error::from_reqwest(e).to_string(), self.policy.backoff(attempt, None), false),
            };
            if attempt >= self.policy.max_tries {
                let msg = format!("gave up after {attempt} tries: {reason}");
                return Err(if throttled { FetchError::Throttled(msg) } else { FetchError::Network(msg) });
            }
            if !control.sleep(wait).await {
                return Err(FetchError::Cancelled);
            }
        }
    }
}

/// What a link resolves with, by site.
pub fn ext_host_of(link: &Link) -> Option<ExtHost> {
    match link.kind {
        LinkKind::DriveFile { .. } | LinkKind::DriveFolder { .. } => Some(ExtHost::Drive),
        LinkKind::MediafireFile { .. } => Some(ExtHost::Mediafire),
        LinkKind::Unsupported { .. } => None,
    }
}

/// Check one link: name, size, liveness, and for a Drive folder the file list.
pub async fn resolve(
    ext: &ExtClient,
    link: &Link,
    control: &Control,
    notify: Notify<'_>,
) -> Result<Resolved, ResolveError> {
    match &link.kind {
        LinkKind::MediafireFile { page_url, .. } => {
            mediafire::resolve(ext, page_url, link.name_hint.as_deref(), control, notify).await
        }
        LinkKind::DriveFile { id } => drive::resolve_file(ext, id, control, notify).await,
        LinkKind::DriveFolder { id } => drive::resolve_folder(ext, id, control, notify).await,
        LinkKind::Unsupported { .. } => Err(ResolveError::Dead(DeadReason::Invalid)),
    }
}

/// `filename` from a `Content-Disposition` value (`filename*=UTF-8''…` wins over `filename=`).
pub fn content_disposition_name(v: &str) -> Option<String> {
    let mut plain = None;
    for part in v.split(';').skip(1) {
        let Some((k, val)) = part.split_once('=') else { continue };
        let (k, val) = (k.trim().to_ascii_lowercase(), val.trim());
        if k == "filename*" {
            // RFC 5987: charset'lang'percent-encoded
            let enc = val.splitn(3, '\'').nth(2).unwrap_or(val);
            return Some(percent_decode(enc));
        } else if k == "filename" {
            plain = Some(val.trim_matches('"').replace("\\\"", "\""));
        }
    }
    plain.filter(|n| !n.is_empty())
}

pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'%'));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Name, total size and Content-Type of a (possibly ranged) download response.
pub struct DownloadMeta {
    pub name: Option<String>,
    pub size: Option<u64>,
    pub content_type: Option<String>,
}

pub fn download_meta(resp: &Response) -> DownloadMeta {
    let h = resp.headers();
    let name = h.get(CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()).and_then(content_disposition_name);
    let size = if resp.status() == StatusCode::PARTIAL_CONTENT {
        h.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(total_from_content_range)
    } else {
        h.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
    };
    let content_type = h.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
    DownloadMeta { name, size, content_type }
}

pub(crate) fn is_html(resp: &Response) -> bool {
    resp.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"))
}

pub(crate) fn location(resp: &Response) -> Option<String> {
    resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// Read at most `limit` bytes of a text body (pages are small; a stray large body is cut).
pub(crate) async fn read_text(mut resp: Response, limit: usize) -> String {
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        buf.extend_from_slice(&chunk);
        if buf.len() >= limit {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_names() {
        assert_eq!(content_disposition_name(r#"attachment; filename="Super Girl.rar""#).as_deref(), Some("Super Girl.rar"));
        assert_eq!(
            content_disposition_name(r#"attachment; filename="x.bin"; filename*=UTF-8''%E3%81%82.zip"#).as_deref(),
            Some("あ.zip")
        );
        assert_eq!(content_disposition_name("attachment; filename=plain.txt").as_deref(), Some("plain.txt"));
        assert_eq!(content_disposition_name("attachment"), None);
        assert_eq!(content_disposition_name("inline; filename=\"\""), None);
    }

    #[test]
    fn dead_reasons_fit_the_row() {
        for r in [DeadReason::Removed, DeadReason::Private, DeadReason::Password, DeadReason::Invalid, DeadReason::Empty] {
            for h in [ExtHost::Drive, ExtHost::Mediafire] {
                assert!(r.text(h).chars().count() <= 38, "{}", r.text(h));
            }
        }
        assert_eq!(DeadReason::Removed.text(ExtHost::Mediafire), "File removed on Mediafire");
    }
}
