//! Mediafire: the file page holds the direct link as the `href` of `<a id="downloadButton">`.
//! The direct link is signed and its lifetime is unmeasured, so it is fetched when needed and
//! used at once, never stored.

use reqwest::header::RANGE;
use reqwest::StatusCode;

use super::{
    download_meta, is_html, location, percent_decode, read_text, DeadReason, ExtClient, ExtHost, Notify, ResolveError,
    Resolved, ResolvedKind,
};
use crate::control::Control;
use crate::links::{attr, decode_entities};

const PAGE_LIMIT_BYTES: usize = 2 * 1024 * 1024;

/// What a Mediafire file page says.
#[derive(Debug, PartialEq, Eq)]
pub enum Page {
    Direct { url: String, name: Option<String> },
    Dead(DeadReason),
}

/// Read a file page. The direct link is the `href` of the `downloadButton` anchor; its file name
/// comes from the page's `filename` div, else from the link.
pub fn parse_file_page(html: &str) -> Page {
    if let Some(at) = html.find("id=\"downloadButton\"") {
        let start = html[..at].rfind("<a").unwrap_or(0);
        let end = html[at..].find('>').map_or(html.len(), |e| at + e + 1);
        if let Some(url) = attr(&html[start..end], "href").filter(|u| u.starts_with("http")) {
            let name = html
                .find("class=\"filename\">")
                .map(|i| &html[i + "class=\"filename\">".len()..])
                .and_then(|r| r.split('<').next())
                .map(|n| decode_entities(n.trim()))
                .filter(|n| !n.is_empty())
                .or_else(|| name_from_url(&url));
            return Page::Direct { url, name };
        }
    }
    let lower = html.to_ascii_lowercase();
    if lower.contains("dl_password") || lower.contains("password protected") || lower.contains("enter the password") {
        Page::Dead(DeadReason::Password)
    } else if lower.contains("file has been removed")
        || lower.contains("invalid or deleted file")
        || lower.contains("the file you attempted to download")
    {
        Page::Dead(DeadReason::Removed)
    } else {
        Page::Dead(DeadReason::Invalid)
    }
}

/// `…/Super+Girl.rar` -> `Super Girl.rar`
fn name_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let leaf = path.rsplit('/').next()?;
    let n = percent_decode(&leaf.replace('+', " "));
    (!n.is_empty()).then_some(n)
}

/// Mediafire sends a removed or invalid key to `/error.php?errno=NNN`.
pub fn dead_from_error_location(loc: &str) -> DeadReason {
    let errno = loc.split("errno=").nth(1).and_then(|r| r.split('&').next());
    match errno {
        Some("320") => DeadReason::Removed, // verified: a deleted key (DAN-1610)
        _ => DeadReason::Invalid,
    }
}

/// Fetch the file page and return the signed direct URL (and the page's file name). Use it at once.
pub async fn direct_url(
    ext: &ExtClient,
    page_url: &str,
    control: &Control,
    notify: Notify<'_>,
) -> Result<(String, Option<String>), ResolveError> {
    let resp = ext.get(ExtHost::Mediafire, control, notify, page_url, &[]).await?;
    let status = resp.status();
    if status.is_redirection() {
        return Err(ResolveError::Dead(location(&resp).map_or(DeadReason::Invalid, |l| dead_from_error_location(&l))));
    }
    if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        return Err(ResolveError::Dead(DeadReason::Removed));
    }
    if !status.is_success() {
        return Err(ResolveError::transient(format!("Mediafire answered HTTP {}", status.as_u16())));
    }
    match parse_file_page(&read_text(resp, PAGE_LIMIT_BYTES).await) {
        Page::Direct { url, name } => Ok((url, name)),
        Page::Dead(r) => Err(ResolveError::Dead(r)),
    }
}

/// Check a Mediafire file: the page for liveness and name, then a ranged request on the direct link
/// for the exact size and Content-Type (the page's size is rounded MiB).
pub async fn resolve(
    ext: &ExtClient,
    page_url: &str,
    hint: Option<&str>,
    control: &Control,
    notify: Notify<'_>,
) -> Result<Resolved, ResolveError> {
    let (direct, page_name) = direct_url(ext, page_url, control, notify).await?;
    let resp = ext.get(ExtHost::Mediafire, control, notify, &direct, &[(RANGE.as_str(), "bytes=0-0".into())]).await?;
    let status = resp.status();
    if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        return Err(ResolveError::Dead(DeadReason::Removed));
    }
    if !(status.is_success()) || is_html(&resp) {
        return Err(ResolveError::transient(format!("Mediafire's download link answered HTTP {}", status.as_u16())));
    }
    let meta = download_meta(&resp);
    let name =
        meta.name.or(page_name).or_else(|| hint.map(str::to_string)).unwrap_or_else(|| "Mediafire file".to_string());
    Ok(Resolved { name, size: meta.size, content_type: meta.content_type, kind: ResolvedKind::File })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/../../fixtures/links/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn example_file_page_gives_the_direct_link_and_the_name() {
        match parse_file_page(&fixture("mediafire-uikej85vz8b2h7z.file-page.html")) {
            Page::Direct { url, name } => {
                assert_eq!(
                    url,
                    "https://download1980.mediafire.com/REDACTED_SIGNED_TOKEN/uikej85vz8b2h7z/Super+Girl.rar"
                );
                assert_eq!(name.as_deref(), Some("Super Girl.rar"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_page_without_a_download_button_is_dead() {
        // The recon's deleted-key page says "Invalid or Deleted File".
        assert_eq!(parse_file_page(&fixture("mediafire-deleted-key.file-page.html")), Page::Dead(DeadReason::Removed));
        assert_eq!(parse_file_page("<html>something else entirely</html>"), Page::Dead(DeadReason::Invalid));
        assert_eq!(parse_file_page("<html>This file is password protected</html>"), Page::Dead(DeadReason::Password));
    }

    #[test]
    fn error_pages_map_to_reasons() {
        assert_eq!(dead_from_error_location("/error.php?errno=320&origin=download"), DeadReason::Removed);
        assert_eq!(dead_from_error_location("/error.php?errno=999"), DeadReason::Invalid);
        assert_eq!(
            name_from_url("https://d.mediafire.com/T/k/Super+Girl%20(1).rar?x=1").as_deref(),
            Some("Super Girl (1).rar")
        );
    }
}
