//! Google Drive: single files (including the "can't scan for viruses" confirmation page) and public
//! folders. A folder's listing is read from the page itself (`_DRIVE_ivd`); there is no API key.

use reqwest::header::RANGE;
use reqwest::{Response, StatusCode};
use serde_json::Value;

use super::{
    download_meta, is_html, location, read_text, Child, DeadReason, ExtClient, ExtHost, Notify, Resolved, ResolvedKind,
    ResolveError,
};
use crate::control::Control;
use crate::links::{attr, decode_entities};

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const PAGE_LIMIT_BYTES: usize = 8 * 1024 * 1024;
/// A folder page that lists this many entries may have been cut: gdown documents 50 as the most a
/// folder page carries, and the recon never saw a folder bigger than 9 (DAN-1610). Treated as "maybe
/// more" until a larger public folder says otherwise (see the PR text for what was measured).
pub const PAGE_LIMIT: usize = 50;
/// At most this many files are taken from one folder (`First 1,000 files only`).
pub const MAX_FILES: usize = 1000;
/// At most this many folder pages are read for one link.
const MAX_FOLDER_PAGES: usize = 100;

/// One entry of a folder page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: Option<u64>,
}

impl Entry {
    pub fn is_folder(&self) -> bool {
        self.mime == FOLDER_MIME
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PageError {
    /// No `_DRIVE_ivd` on the page: a sign-in wall, an error page or a layout we do not know.
    NoListing,
    Malformed(String),
}

/// Undo the JavaScript string escapes Google uses (`\x22`, `\/`, `é`, …).
fn js_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(e) = it.next() else { break };
        let mut hex = |n: usize| -> Option<u32> {
            let h: String = (0..n).filter_map(|_| it.next()).collect();
            u32::from_str_radix(&h, 16).ok()
        };
        match e {
            'x' => out.extend(hex(2).and_then(char::from_u32)),
            'u' => out.extend(hex(4).and_then(char::from_u32)),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            other => out.push(other), // \\ \/ \' \"
        }
    }
    out
}

/// The `_DRIVE_ivd` JS string of a folder page, decoded to JSON.
fn ivd_json(html: &str) -> Option<String> {
    let marker = "window['_DRIVE_ivd'] = '";
    let rest = &html[html.find(marker)? + marker.len()..];
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\'' => return Some(js_unescape(&rest[..i])),
            _ => i += 1,
        }
    }
    None
}

fn size_of(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Entries of a folder page, in the order Drive lists them. Row = `[id, [parent], name, mime, …, size at 13]`.
pub fn parse_folder_page(html: &str) -> Result<Vec<Entry>, PageError> {
    let json = ivd_json(html).ok_or(PageError::NoListing)?;
    let top: Value = serde_json::from_str(&json).map_err(|e| PageError::Malformed(e.to_string()))?;
    let rows = match top.get(0) {
        Some(Value::Array(rows)) => rows.as_slice(),
        Some(Value::Null) | None => &[],
        Some(_) => return Err(PageError::Malformed("rows are not a list".into())),
    };
    Ok(rows
        .iter()
        .filter_map(|r| {
            let id = r.get(0)?.as_str()?.to_string();
            let name = r.get(2)?.as_str()?.to_string();
            let mime = r.get(3)?.as_str()?.to_string();
            Some(Entry { id, name, mime, size: size_of(r.get(13)) })
        })
        .collect())
}

/// The folder's own name: `<title>Super Girl - Google Drive</title>`.
pub fn folder_title(html: &str) -> Option<String> {
    let t = html.split("<title>").nth(1)?.split("</title>").next()?;
    let t = decode_entities(t.trim());
    let t = t.strip_suffix(" - Google Drive").unwrap_or(&t).trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// The virus-scan page's form: where to send it and its hidden fields (`id`, `export`, `confirm`, `uuid`).
#[derive(Debug, PartialEq, Eq)]
pub struct ConfirmForm {
    pub action: String,
    pub fields: Vec<(String, String)>,
}

impl ConfirmForm {
    /// `action?id=…&export=download&confirm=t&uuid=…`
    pub fn url(&self) -> Option<String> {
        let mut u = url::Url::parse(&self.action).ok()?;
        u.query_pairs_mut().extend_pairs(self.fields.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        Some(u.to_string())
    }
}

/// Read the "Google Drive can't scan this file for viruses" page. Every hidden field is submitted:
/// `confirm=t` alone was seen working once and is not reliable (DAN-1610 addendum).
pub fn parse_confirm_form(html: &str) -> Option<ConfirmForm> {
    let at = html.find("id=\"download-form\"").or_else(|| html.find("name=\"confirm\"").and_then(|i| html[..i].rfind("<form")))?;
    let start = html[..at].rfind("<form").unwrap_or(at);
    let end = html[start..].find("</form>").map_or(html.len(), |e| start + e);
    let form = &html[start..end];
    let head = &form[..form.find('>').unwrap_or(form.len())];
    let action = attr(head, "action")?;
    let mut fields = Vec::new();
    let mut rest = form;
    while let Some(i) = rest.find("<input") {
        rest = &rest[i + 1..];
        let tag = &rest[..rest.find('>').unwrap_or(rest.len())];
        if attr(tag, "type").is_some_and(|t| t.eq_ignore_ascii_case("hidden")) {
            if let (Some(n), Some(v)) = (attr(tag, "name"), attr(tag, "value")) {
                fields.push((n, v));
            }
        }
    }
    (!fields.is_empty()).then_some(ConfirmForm { action, fields })
}

fn page_says_quota(html: &str) -> bool {
    let l = html.to_ascii_lowercase();
    l.contains("quota exceeded") || l.contains("too many users have viewed or downloaded") || l.contains("download quota")
}

fn page_says_private(html: &str) -> bool {
    let l = html.to_ascii_lowercase();
    l.contains("you need access") || l.contains("request access") || l.contains("accounts.google.com/servicelogin")
}

/// `https://drive.google.com/uc?export=download&id=…`
pub fn file_url(id: &str) -> String {
    format!("https://drive.google.com/uc?export=download&id={id}")
}

fn folder_page_url(id: &str) -> String {
    format!("https://drive.google.com/drive/folders/{id}")
}

/// Open a download response for file `id`: follows the redirect to `drive.usercontent.google.com` and,
/// when Drive answers with the virus-scan page, submits its form. `range` is sent as the Range header.
/// The returned response has a 2xx status and is not an HTML page.
pub async fn open_file(
    ext: &ExtClient,
    id: &str,
    range: Option<String>,
    control: &Control,
    notify: Notify<'_>,
) -> Result<Response, ResolveError> {
    let extra: Vec<(&str, String)> = range.map(|r| (RANGE.as_str(), r)).into_iter().collect();
    let mut url = file_url(id);
    for _ in 0..2 {
        let resp = ext.get(ExtHost::Drive, control, notify, &url, &extra).await?;
        let status = resp.status();
        if status.is_redirection() {
            let private = location(&resp).is_some_and(|l| l.contains("accounts.google.com"));
            return Err(ResolveError::Dead(if private { DeadReason::Private } else { DeadReason::Invalid }));
        }
        if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
            return Err(ResolveError::Dead(DeadReason::Removed));
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(ResolveError::Dead(DeadReason::Private));
        }
        if status == StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok(resp); // the caller owns the resume logic
        }
        if !status.is_success() {
            return Err(ResolveError::transient(format!("Google Drive answered HTTP {}", status.as_u16())));
        }
        if !is_html(&resp) {
            return Ok(resp);
        }
        let body = read_text(resp, PAGE_LIMIT_BYTES).await;
        if let Some(next) = parse_confirm_form(&body).and_then(|f| f.url()) {
            url = next;
            continue;
        }
        return Err(if page_says_quota(&body) {
            ResolveError::Transient { message: "Google Drive download limit reached".into(), quota: true }
        } else if page_says_private(&body) {
            ResolveError::Dead(DeadReason::Private)
        } else {
            ResolveError::transient("Google Drive sent a page we do not recognise")
        });
    }
    Err(ResolveError::transient("Google Drive kept asking for confirmation"))
}

/// Check a single Drive file: a one-byte ranged download request gives the real name (from
/// `Content-Disposition`), the exact size and the Content-Type.
pub async fn resolve_file(ext: &ExtClient, id: &str, control: &Control, notify: Notify<'_>) -> Result<Resolved, ResolveError> {
    let resp = open_file(ext, id, Some("bytes=0-0".into()), control, notify).await?;
    let meta = download_meta(&resp);
    Ok(Resolved {
        name: meta.name.unwrap_or_else(|| "Google Drive file".to_string()),
        size: meta.size,
        content_type: meta.content_type,
        kind: ResolvedKind::File,
    })
}

/// Fetch one folder page.
async fn folder_page(
    ext: &ExtClient,
    id: &str,
    control: &Control,
    notify: Notify<'_>,
) -> Result<(Option<String>, Vec<Entry>), ResolveError> {
    let resp = ext.get(ExtHost::Drive, control, notify, &folder_page_url(id), &[]).await?;
    let status = resp.status();
    if status.is_redirection() {
        return Err(ResolveError::Dead(DeadReason::Private));
    }
    if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        return Err(ResolveError::Dead(DeadReason::Removed));
    }
    if !status.is_success() {
        return Err(ResolveError::transient(format!("Google Drive answered HTTP {}", status.as_u16())));
    }
    let html = read_text(resp, PAGE_LIMIT_BYTES).await;
    match parse_folder_page(&html) {
        Ok(entries) => Ok((folder_title(&html), entries)),
        Err(PageError::NoListing) if page_says_private(&html) || html.contains("Sign in") => {
            Err(ResolveError::Dead(DeadReason::Private))
        }
        Err(e) => Err(ResolveError::transient(format!("could not read the Google Drive folder page ({e:?})"))),
    }
}

/// Check a Drive folder: read its page and every sub-folder's page. The children come out as the
/// folder's own files first, then each sub-folder in listing order, each with its path below the
/// shared folder. `partial` is set unless Drive's own end-of-list signal was seen (see [`PAGE_LIMIT`]).
pub async fn resolve_folder(ext: &ExtClient, id: &str, control: &Control, notify: Notify<'_>) -> Result<Resolved, ResolveError> {
    let mut children: Vec<Child> = Vec::new();
    let mut partial = false;
    let mut root_name = None;
    let mut visited = std::collections::HashSet::new();
    // (folder id, path of folder names below the root); a stack, so sub-folders are depth-first in order.
    let mut stack: Vec<(String, Vec<String>)> = vec![(id.to_string(), Vec::new())];
    let mut pages = 0;
    while let Some((fid, dirs)) = stack.pop() {
        if !visited.insert(fid.clone()) {
            continue;
        }
        if pages >= MAX_FOLDER_PAGES {
            partial = true;
            break;
        }
        pages += 1;
        let (title, entries) = match folder_page(ext, &fid, control, notify).await {
            Ok(r) => r,
            // A sub-folder we cannot read leaves the listing short: say so instead of hiding it.
            Err(ResolveError::Dead(_)) if !dirs.is_empty() => {
                partial = true;
                continue;
            }
            Err(e) => return Err(e),
        };
        if dirs.is_empty() {
            root_name = title;
        }
        if entries.len() >= PAGE_LIMIT {
            partial = true;
        }
        let mut subfolders = Vec::new();
        for e in entries {
            if e.is_folder() {
                subfolders.push(e);
            } else if e.mime.starts_with("application/vnd.google-apps.") {
                // Docs, Sheets, shortcuts: no file to download.
            } else if children.len() >= MAX_FILES {
                partial = true;
            } else {
                children.push(Child { id: e.id, dirs: dirs.clone(), name: e.name, size: e.size, mime: Some(e.mime) });
            }
        }
        for f in subfolders.into_iter().rev() {
            let mut d = dirs.clone();
            d.push(f.name);
            stack.push((f.id, d));
        }
    }
    if children.is_empty() && !partial {
        return Err(ResolveError::Dead(DeadReason::Empty));
    }
    let known: Vec<u64> = children.iter().filter_map(|c| c.size).collect();
    let size = (!known.is_empty()).then(|| known.iter().sum());
    Ok(Resolved {
        name: root_name.unwrap_or_else(|| "Google Drive folder".to_string()),
        size,
        content_type: None,
        kind: ResolvedKind::Folder { children, partial },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/../../fixtures/links/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn the_example_folder_page_lists_two_files_and_four_subfolders() {
        let html = fixture("drive-folder-14tXDG5k.html");
        let entries = parse_folder_page(&html).unwrap();
        let names: Vec<(&str, bool, Option<u64>)> = entries.iter().map(|e| (e.name.as_str(), e.is_folder(), e.size)).collect();
        assert_eq!(
            names,
            [
                ("Jpg process", true, None),
                ("Lineart", true, None),
                ("nsfw", true, None),
                ("PNG", true, None),
                ("Super Girl.jpg", false, Some(2_645_191)),
                ("Super Girl.psd", false, Some(89_567_214)),
            ]
        );
        assert_eq!(folder_title(&html).as_deref(), Some("Super Girl"));
        assert!(entries.len() < PAGE_LIMIT);
    }

    #[test]
    fn the_decoded_listing_agrees_with_the_recon_json() {
        // The recon decoded the same JS string by hand (and redacted owner details); ids, names, types
        // and sizes must agree.
        let rows = |v: Value| -> Vec<(String, String, String, Value)> {
            v[0].as_array()
                .unwrap()
                .iter()
                .map(|r| (r[0].to_string(), r[2].to_string(), r[3].to_string(), r[13].clone()))
                .collect()
        };
        let want: Value = serde_json::from_str(&fixture("drive-folder-14tXDG5k.ivd.json")).unwrap();
        let got: Value = serde_json::from_str(&ivd_json(&fixture("drive-folder-14tXDG5k.html")).unwrap()).unwrap();
        assert_eq!(rows(got), rows(want));
    }

    #[test]
    fn a_404_page_has_no_listing() {
        assert_eq!(parse_folder_page(&fixture("drive-folder-404.html")), Err(PageError::NoListing));
    }

    #[test]
    fn virus_scan_page_form_keeps_every_hidden_field() {
        let f = parse_confirm_form(&fixture("drive-virus-scan-confirm.html")).unwrap();
        assert_eq!(f.action, "https://drive.usercontent.google.com/download");
        let names: Vec<&str> = f.fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["id", "export", "confirm", "uuid"]);
        let url = f.url().unwrap();
        assert!(url.starts_with("https://drive.usercontent.google.com/download?id=0B7EVK8r0v71pZjFTYXZWM3FlRnM&export=download&confirm=t&uuid="), "{url}");
        assert!(parse_confirm_form("<html>no form</html>").is_none());
    }

    #[test]
    fn js_escapes_decode() {
        assert_eq!(js_unescape(r"\x5b\x22a\x22\x5d \/ é \\ \'"), "[\"a\"] / é \\ '");
    }

    #[test]
    fn quota_and_private_pages_are_told_apart() {
        assert!(page_says_quota("<p>Sorry, you can't view or download this file at this time.</p> Too many users have viewed or downloaded this file recently."));
        assert!(page_says_private("<h1>You need access</h1>"));
        assert!(!page_says_quota("<html>ok</html>"));
    }
}
