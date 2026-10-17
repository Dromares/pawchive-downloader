//! External links found in post text (DAN-1609): extraction from the post body, host labelling and
//! de-duplication. Everything here is pure; nothing touches the network.

use std::collections::HashMap;

use url::Url;

use crate::model::Post;

/// Where a link points. Only [`Host::Drive`] and [`Host::Mediafire`] can be downloaded.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    Drive,
    Mediafire,
    Mega,
    Dropbox,
    /// The lower-cased host without `www.`.
    Other(String),
}

impl Host {
    /// Display name used in the UI and the export (`Google Drive`, `Mediafire`, `Mega`, `Dropbox`,
    /// else the bare domain).
    pub fn name(&self) -> String {
        match self {
            Host::Drive => "Google Drive".into(),
            Host::Mediafire => "Mediafire".into(),
            Host::Mega => "Mega".into(),
            Host::Dropbox => "Dropbox".into(),
            Host::Other(h) => h.clone(),
        }
    }

    fn from_domain(domain: &str) -> Host {
        let d = domain.strip_prefix("www.").unwrap_or(domain);
        match d {
            "drive.google.com" | "docs.google.com" | "drive.usercontent.google.com" => Host::Drive,
            "mega.nz" | "mega.io" | "mega.co.nz" => Host::Mega,
            "dropbox.com" | "dropboxusercontent.com" => Host::Dropbox,
            _ if d == "mediafire.com" || d.ends_with(".mediafire.com") => Host::Mediafire,
            _ => Host::Other(d.to_string()),
        }
    }
}

/// What a link is, as far as downloading is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkKind {
    DriveFile {
        id: String,
    },
    DriveFolder {
        id: String,
    },
    /// `page_url` is the file page that holds the direct link.
    MediafireFile {
        key: String,
        page_url: String,
    },
    /// Nothing can be downloaded from it. `why` is the second line of the row
    /// (`Mega isn't supported`).
    Unsupported {
        why: String,
    },
}

/// The post a link was found in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostRef {
    pub id: String,
    pub title: String,
    pub published: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// Index in the scan's link list; stable for the whole session.
    pub id: usize,
    /// Exactly as written in the first post it was found in (entity-decoded, not canonicalised).
    pub url: String,
    pub host: Host,
    pub kind: LinkKind,
    /// A file name read from the URL itself (`Super_Girl.rar`). A hint until the host says more.
    pub name_hint: Option<String>,
    /// Every post that holds the link, newest first. Never empty.
    pub posts: Vec<PostRef>,
}

impl Link {
    pub fn downloadable(&self) -> bool {
        !matches!(self.kind, LinkKind::Unsupported { .. })
    }

    /// The post shown in the row: the newest one.
    pub fn source(&self) -> &PostRef {
        &self.posts[0]
    }
}

/// Entity-decode the handful of entities that occur in URLs and text.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest.find(';').filter(|&e| e <= 10).and_then(|e| {
            let ent = &rest[1..e];
            let ch = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => ent.strip_prefix('#').and_then(|n| {
                    let code = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok()?,
                        None => n.parse().ok()?,
                    };
                    char::from_u32(code)
                }),
            }?;
            Some((ch, e + 1))
        });
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_url_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | '\u{a0}' | '\u{200b}')
}

/// Drop sentence punctuation and unbalanced closing brackets from the end of a bare URL.
fn trim_url_tail(mut s: &str) -> &str {
    loop {
        let Some(last) = s.chars().next_back() else { return s };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '*' | '_' | '~' => true,
            ')' => s.matches('(').count() < s.matches(')').count(),
            ']' => s.matches('[').count() < s.matches(']').count(),
            '}' => s.matches('{').count() < s.matches('}').count(),
            _ => false,
        };
        if !strip {
            return s;
        }
        s = &s[..s.len() - last.len_utf8()];
    }
}

/// Bare `http(s)://…` URLs in already-decoded text.
fn scan_text(text: &str, out: &mut Vec<String>) {
    let text = decode_entities(text);
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("http") {
        let start = from + rel;
        let tail = &lower[start..];
        let scheme = if tail.starts_with("https://") {
            8
        } else if tail.starts_with("http://") {
            7
        } else {
            from = start + 4;
            continue;
        };
        let end = text[start + scheme..].find(is_url_end).map_or(text.len(), |e| start + scheme + e);
        let url = trim_url_tail(&text[start..end]);
        if url.len() > scheme {
            out.push(url.to_string());
        }
        from = end.max(start + scheme);
    }
}

/// The value of attribute `name` in the tag source `tag` (`<a href="…">`), entity-decoded.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(name) {
        let at = from + rel;
        from = at + name.len();
        let before_ok = lower[..at].chars().next_back().is_some_and(|c| c.is_whitespace() || c == '"' || c == '\'');
        if !before_ok {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let value = match rest.chars().next()? {
            q @ ('"' | '\'') => {
                let inner = &rest[1..];
                &inner[..inner.find(q).unwrap_or(inner.len())]
            }
            _ => &rest[..rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len())],
        };
        return Some(decode_entities(value.trim()));
    }
    None
}

/// Every URL a post body points at: `<a href>` and `<iframe src>` targets plus bare URLs in the
/// text. Order of appearance, duplicates kept (the caller de-duplicates by canonical key).
pub fn extract_urls(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let (mut i, mut text_from) = (0, 0);
    while i < bytes.len() {
        let is_tag = bytes[i] == b'<'
            && bytes.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'/' || *c == b'!');
        if !is_tag {
            i += 1;
            continue;
        }
        scan_text(&body[text_from..i], &mut out);
        // The end of the tag, skipping `>` inside quoted attribute values.
        let mut j = i + 1;
        let mut quote: Option<u8> = None;
        while j < bytes.len() {
            match (quote, bytes[j]) {
                (None, b'>') => break,
                (None, q @ (b'"' | b'\'')) => quote = Some(q),
                (Some(q), c) if c == q => quote = None,
                _ => {}
            }
            j += 1;
        }
        let end = (j + 1).min(bytes.len());
        let tag = &body[i..end];
        let name: String = tag[1..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        let value = match name.to_ascii_lowercase().as_str() {
            "a" => attr(tag, "href"),
            "iframe" | "embed" => attr(tag, "src"),
            _ => None,
        };
        out.extend(value.filter(|v| !v.is_empty()));
        i = end;
        text_from = end;
    }
    scan_text(&body[text_from.min(body.len())..], &mut out);
    out
}

fn percent_decode(s: &str) -> String {
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

fn looks_like_file_name(s: &str) -> bool {
    crate::classify::extension_of(s).is_some()
}

/// Classify one URL. `None` for anything that is not an external http(s) link (Pawchive's own
/// hosts, `mailto:`, relative paths, …). Returns `(host, kind, name hint, canonical key)`.
pub fn classify_url(raw: &str) -> Option<(Host, LinkKind, Option<String>, String)> {
    let url = Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let domain = url.host_str()?.to_ascii_lowercase();
    if domain == "pawchive.pw" || domain.ends_with(".pawchive.pw") {
        return None;
    }
    let host = Host::from_domain(&domain);
    let segs: Vec<String> = url.path_segments().map(|s| s.filter(|x| !x.is_empty()).map(percent_decode).collect()).unwrap_or_default();
    let query = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).filter(|v| !v.is_empty());
    let unsupported = |why: String, key: String| (host.clone(), LinkKind::Unsupported { why }, None, key);
    let generic_key = || {
        let mut u = url.clone();
        u.set_fragment(None);
        u.as_str().trim_end_matches('/').to_string()
    };
    let (host_, kind, hint, key) = match &host {
        Host::Drive => {
            let seg = |i: usize| segs.get(i).map(String::as_str);
            let after = |marker: &str| segs.iter().position(|s| s == marker).and_then(|p| segs.get(p + 1)).cloned();
            let folder = if domain == "docs.google.com" { None } else { after("folders") };
            let doc_kind = matches!(seg(0), Some("document" | "spreadsheets" | "presentation" | "forms" | "drawings"));
            if domain == "docs.google.com" && doc_kind {
                unsupported("Google Docs files aren't supported".into(), generic_key())
            } else if let Some(id) = folder.or_else(|| (seg(0) == Some("folderview")).then(|| query("id")).flatten()) {
                (host.clone(), LinkKind::DriveFolder { id: id.clone() }, None, format!("drive:folder:{id}"))
            } else if let Some(id) = after("d").filter(|_| segs.iter().any(|s| s == "file")).or_else(|| {
                (matches!(seg(0), Some("open" | "uc")) || seg(2) == Some("uc")).then(|| query("id")).flatten()
            }) {
                (host.clone(), LinkKind::DriveFile { id: id.clone() }, None, format!("drive:file:{id}"))
            } else {
                unsupported("This Google Drive link isn't supported".into(), generic_key())
            }
        }
        Host::Mediafire => {
            let direct = domain.starts_with("download");
            let key = match segs.first().map(String::as_str) {
                Some("file" | "view" | "download" | "file_premium") => segs.get(1).cloned(),
                Some("folder" | "folders") => None,
                _ if direct => segs.get(1).cloned(),
                // Legacy `mediafire.com/?quickkey`.
                _ => url.query().filter(|q| !q.contains('=') && !q.is_empty()).map(str::to_string),
            };
            match (key, segs.first().map(String::as_str)) {
                (Some(key), _) if key.chars().all(|c| c.is_ascii_alphanumeric()) => {
                    let hint = if direct {
                        segs.last().filter(|n| looks_like_file_name(n)).map(|n| n.replace('+', " "))
                    } else {
                        segs.get(2).filter(|n| n.as_str() != "file" && looks_like_file_name(n)).cloned()
                    };
                    // A direct download URL carries a signed token that we neither keep nor reuse.
                    let page_url = if direct || segs.first().map(String::as_str) != Some("file") {
                        format!("https://www.mediafire.com/file/{key}/file")
                    } else {
                        let mut u = url.clone();
                        u.set_fragment(None);
                        u.as_str().to_string()
                    };
                    (host.clone(), LinkKind::MediafireFile { key: key.clone(), page_url }, hint, format!("mf:{key}"))
                }
                (None, Some("folder" | "folders")) => {
                    unsupported("Mediafire folders aren't supported".into(), generic_key())
                }
                _ => unsupported("This Mediafire link isn't supported".into(), generic_key()),
            }
        }
        Host::Dropbox => {
            let hint = segs.last().filter(|n| looks_like_file_name(n)).cloned();
            let (h, k, _, key) = unsupported("Dropbox isn't supported".into(), generic_key());
            (h, k, hint, key)
        }
        other => unsupported(format!("{} isn't supported", other.name()), generic_key()),
    };
    Some((host_, kind, hint, key))
}

/// All links in `posts`, de-duplicated and in display order: downloadable hosts first in scan
/// order, then unsupported hosts grouped by host (most links first, ties by first appearance).
/// The order depends only on the scan, so it never changes while the app runs.
pub fn collect(posts: &[Post]) -> Vec<Link> {
    let mut links: Vec<Link> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for post in posts {
        let mut urls = extract_urls(&post.content);
        if let Some(e) = &post.embed {
            if !e.url.trim().is_empty() {
                urls.push(e.url.trim().to_string());
            }
            urls.extend(extract_urls(&e.description));
        }
        for u in urls {
            let Some((host, kind, name_hint, key)) = classify_url(&u) else { continue };
            let pref = PostRef { id: post.id.clone(), title: post.title.clone(), published: post.published.clone() };
            match by_key.get(&key) {
                Some(&i) => {
                    if !links[i].posts.iter().any(|p| p.id == pref.id) {
                        links[i].posts.push(pref);
                    }
                    if links[i].name_hint.is_none() {
                        links[i].name_hint = name_hint;
                    }
                }
                None => {
                    by_key.insert(key, links.len());
                    links.push(Link { id: links.len(), url: u.trim().to_string(), host, kind, name_hint, posts: vec![pref] });
                }
            }
        }
    }
    for l in &mut links {
        // Newest first; the sort is stable, so undated or equal posts keep scan order.
        l.posts.sort_by(|a, b| b.published.cmp(&a.published));
    }
    // Unsupported hosts: most links first, ties by first appearance.
    let mut host_rank: Vec<(Host, usize, usize)> = Vec::new();
    for (i, l) in links.iter().enumerate().filter(|(_, l)| !l.downloadable()) {
        match host_rank.iter_mut().find(|(h, _, _)| *h == l.host) {
            Some((_, n, _)) => *n += 1,
            None => host_rank.push((l.host.clone(), 1, i)),
        }
    }
    let rank = |l: &Link| -> (usize, usize, usize) {
        if l.downloadable() {
            return (0, 0, 0);
        }
        let pos = host_rank.iter().position(|(h, _, _)| *h == l.host);
        let (_, n, first) = pos.map(|p| &host_rank[p]).cloned().unwrap_or((l.host.clone(), 0, 0));
        (1, usize::MAX - n, first)
    };
    let mut order: Vec<usize> = (0..links.len()).collect();
    order.sort_by_key(|&i| rank(&links[i])); // stable: scan order inside a rank
    let mut sorted: Vec<Link> = order.into_iter().map(|i| links[i].clone()).collect();
    for (i, l) in sorted.iter_mut().enumerate() {
        l.id = i;
    }
    sorted
}

/// Number of distinct posts that hold at least one link.
pub fn posts_with_links(links: &[Link]) -> usize {
    links.iter().flat_map(|l| l.posts.iter().map(|p| p.id.as_str())).collect::<std::collections::HashSet<_>>().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse_post;

    fn post(id: &str, published: &str, content: &str) -> Post {
        Post {
            id: id.into(),
            title: format!("Post {id}"),
            published: Some(published.into()),
            file: None,
            attachments: vec![],
            content: content.into(),
            embed: None,
        }
    }

    #[test]
    fn finds_anchors_and_bare_urls_and_decodes_entities() {
        let html = r#"<p>Get it: <a href="https://www.mediafire.com/file/abc123/x.zip/file">MF</a></p>
            <p>https://mega.nz/folder/Kp3a9QxZ#key, and (https://example.com/a_(b)) then
            https://drive.google.com/file/d/AAA/view?usp=sharing&amp;x=1.</p>
            <a href='https://a.test/p?a=1&amp;b=2'>x</a><img src="https://img.test/x.png">mailto:me@x.org"#;
        let urls = extract_urls(html);
        assert_eq!(
            urls,
            [
                "https://www.mediafire.com/file/abc123/x.zip/file",
                "https://mega.nz/folder/Kp3a9QxZ#key",
                "https://example.com/a_(b)",
                "https://drive.google.com/file/d/AAA/view?usp=sharing&x=1",
                "https://a.test/p?a=1&b=2",
            ]
        );
    }

    #[test]
    fn an_href_is_not_listed_twice_when_it_is_also_the_link_text() {
        let post = post("1", "2026-01-01", r#"<a href="https://mega.nz/file/X#k">https://mega.nz/file/X#k</a>"#);
        let links = collect(&[post]);
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn labels_hosts_and_kinds() {
        let c = |u: &str| classify_url(u).unwrap();
        let (h, k, hint, key) = c("https://www.mediafire.com/file/uikej85vz8b2h7z/Super_Girl.rar/file");
        assert_eq!(h.name(), "Mediafire");
        assert_eq!(hint.as_deref(), Some("Super_Girl.rar"));
        assert_eq!(key, "mf:uikej85vz8b2h7z");
        assert!(matches!(k, LinkKind::MediafireFile { ref key, .. } if key == "uikej85vz8b2h7z"));
        assert!(matches!(c("https://drive.google.com/drive/u/0/folders/F1?usp=sharing").1, LinkKind::DriveFolder { id } if id == "F1"));
        assert!(matches!(c("https://drive.google.com/file/d/F2/view").1, LinkKind::DriveFile { id } if id == "F2"));
        assert!(matches!(c("https://drive.google.com/open?id=F3").1, LinkKind::DriveFile { id } if id == "F3"));
        assert!(matches!(c("https://drive.google.com/uc?export=download&id=F4").1, LinkKind::DriveFile { id } if id == "F4"));
        assert_eq!(c("https://drive.google.com/uc?id=F4").3, c("https://drive.google.com/file/d/F4/view").3);
        assert_eq!(c("https://mega.nz/file/abc#key").0.name(), "Mega");
        let (h, k, hint, _) = c("https://www.dropbox.com/s/xyz/Bonus_Wallpapers.zip?dl=0");
        assert_eq!((h.name().as_str(), hint.as_deref()), ("Dropbox", Some("Bonus_Wallpapers.zip")));
        assert!(matches!(k, LinkKind::Unsupported { ref why } if why == "Dropbox isn't supported"));
        assert_eq!(c("https://www.wetransfer.com/downloads/x").0.name(), "wetransfer.com");
        assert!(matches!(c("https://www.mediafire.com/folder/k1/stuff").1, LinkKind::Unsupported { .. }));
        assert!(matches!(c("https://docs.google.com/document/d/D/edit").1, LinkKind::Unsupported { .. }));
        // A signed direct URL becomes a file-page link and drops the token.
        let (_, k, hint, key) = c("https://download1980.mediafire.com/TOKEN/uikej85vz8b2h7z/Super+Girl.rar");
        assert_eq!(key, "mf:uikej85vz8b2h7z");
        assert_eq!(hint.as_deref(), Some("Super Girl.rar"));
        assert!(matches!(k, LinkKind::MediafireFile { page_url, .. } if !page_url.contains("TOKEN")));
        // Not external.
        for u in ["https://pawchive.pw/patreon/user/1", "https://file.pawchive.pw/data/x.jpg", "mailto:a@b.c", "/relative", "ftp://x.test/a"] {
            assert!(classify_url(u).is_none(), "{u}");
        }
    }

    #[test]
    fn same_link_in_many_posts_is_one_row_with_newest_post_first() {
        let u = r#"<a href="https://drive.google.com/drive/folders/F1">d</a>"#;
        let posts = [post("3", "2026-03-01", u), post("1", "2026-01-01", u), post("2", "2026-02-01", &format!("{u}{u}"))];
        let links = collect(&posts);
        assert_eq!(links.len(), 1);
        let ids: Vec<&str> = links[0].posts.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["3", "2", "1"], "newest first, the same post twice counts once");
        assert_eq!(links[0].source().id, "3");
        assert_eq!(posts_with_links(&links), 3);
    }

    #[test]
    fn display_order_is_downloadable_first_then_unsupported_grouped_by_host() {
        let body = |urls: &[&str]| urls.iter().map(|u| format!(r#"<a href="{u}">x</a>"#)).collect::<String>();
        let posts = [
            post("4", "2026-04-04", &body(&["https://mega.nz/file/1", "https://patreon.com/a", "https://www.mediafire.com/file/k1/a.zip/file"])),
            post("3", "2026-03-03", &body(&["https://patreon.com/b", "https://ko-fi.com/c", "https://drive.google.com/drive/folders/F1"])),
            post("2", "2026-02-02", &body(&["https://patreon.com/c", "https://mega.nz/file/2"])),
        ];
        let links = collect(&posts);
        let order: Vec<String> = links.iter().map(|l| format!("{}:{}", l.id, l.host.name())).collect();
        assert_eq!(
            order,
            ["0:Mediafire", "1:Google Drive", "2:patreon.com", "3:patreon.com", "4:patreon.com", "5:Mega", "6:Mega", "7:ko-fi.com"]
        );
    }

    #[test]
    fn embed_url_counts_and_a_missing_body_is_fine() {
        let mut p = post("1", "2026-01-01", "");
        p.embed = Some(crate::model::RawEmbed { url: "https://youtu.be/abc".into(), description: String::new() });
        let links = collect(&[p]);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].host.name(), "youtu.be");
    }

    /// The example post of DAN-1609: the `1.jpg` attachment, a Drive folder and a Mediafire file.
    #[test]
    fn example_post_yields_the_image_and_both_links_labelled() {
        let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/links/post-162546844.api.json")).unwrap();
        let post = parse_post(&bytes).unwrap();
        let files = post.files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "1.jpg");
        let links = collect(&[post]);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].host.name(), "Google Drive");
        assert_eq!(links[0].url, "https://drive.google.com/drive/folders/14tXDG5kTah5Vkbn3Pp2Z68rcCTUrRMaQ?usp=sharing");
        assert!(matches!(&links[0].kind, LinkKind::DriveFolder { id } if id == "14tXDG5kTah5Vkbn3Pp2Z68rcCTUrRMaQ"));
        assert_eq!(links[1].host.name(), "Mediafire");
        assert_eq!(links[1].name_hint.as_deref(), Some("Super_Girl.rar"));
        assert!(links.iter().all(|l| l.downloadable() && l.source().id == "162546844"));
    }

    #[test]
    fn creator_listing_page_carries_links_too() {
        let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/links/creator-5163822.list-o0.api.json")).unwrap();
        let posts = crate::model::parse_posts(&bytes).unwrap();
        assert_eq!(posts.len(), 50);
        let links = collect(&posts);
        assert!(links.iter().any(|l| l.host == Host::Drive), "{links:#?}");
        assert!(links.iter().any(|l| l.host == Host::Mediafire));
        assert_eq!(links.iter().map(|l| l.id).collect::<Vec<_>>(), (0..links.len()).collect::<Vec<_>>());
    }

    #[test]
    fn entities_decode() {
        assert_eq!(decode_entities("a&amp;b &lt;c&gt; &#39;q&#x27; &#12354; &bogus; &"), "a&b <c> 'q' あ &bogus; &");
    }
}
