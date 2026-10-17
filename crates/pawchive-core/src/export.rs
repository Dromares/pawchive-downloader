//! The "Export list" files: every link found in a scan, as plain text or CSV (DAN-1609).
//! Pure functions; the GUI picks the file name and writes the bytes.

use crate::links::Link;

/// Where a link stands, in the vocabulary of the CSV `status` column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportStatus {
    NotChecked,
    Ready,
    NotDownloadable,
    Dead,
    Queued,
    Saved,
    Failed,
}

impl ExportStatus {
    pub fn code(self) -> &'static str {
        match self {
            ExportStatus::NotChecked => "not_checked",
            ExportStatus::Ready => "ready",
            ExportStatus::NotDownloadable => "not_downloadable",
            ExportStatus::Dead => "dead",
            ExportStatus::Queued => "queued",
            ExportStatus::Saved => "saved",
            ExportStatus::Failed => "failed",
        }
    }
}

/// What the app knows about a link beyond the scan: filled in by the caller per link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportInfo {
    pub status: ExportStatus,
    /// Resolved name, else the name read from the URL.
    pub filename: Option<String>,
    pub size: Option<u64>,
    /// `removed | private | password | invalid | empty | quota | listing_incomplete | unsupported_host`.
    pub note: Option<&'static str>,
}

pub const CSV_HEADER: &str =
    "url,host,downloadable,filename,size_bytes,status,service,creator_id,post_id,post_title,post_date,post_url,note";

/// One URL per line, unique, in scan order, exactly as written in the post. LF, no BOM, no header.
pub fn to_txt(links: &[Link]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out = String::new();
    for l in links {
        if seen.insert(l.url.as_str()) {
            out.push_str(&l.url);
            out.push('\n');
        }
    }
    out
}

/// Text from the outside world must not start a spreadsheet formula.
fn defuse(s: &str) -> String {
    match s.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') => format!("'{s}"),
        _ => s.to_string(),
    }
}

fn field(s: &str) -> String {
    let s = defuse(s);
    if s.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

/// RFC 4180 CSV with a UTF-8 BOM (Fanbox titles are Japanese and Excel needs it) and CRLF line ends.
/// One row per link and post that holds it, so the same URL in three posts is three rows.
pub fn to_csv(links: &[Link], service: &str, creator_id: &str, info: &dyn Fn(&Link) -> ExportInfo) -> Vec<u8> {
    let mut out = String::from('\u{feff}');
    out.push_str(CSV_HEADER);
    out.push_str("\r\n");
    for l in links {
        let i = info(l);
        for p in &l.posts {
            let date = p.published.as_deref().and_then(|d| d.get(..10)).unwrap_or("");
            let cols = [
                field(&l.url),
                field(&l.host.name()),
                if l.downloadable() { "yes" } else { "no" }.to_string(),
                field(i.filename.as_deref().unwrap_or("")),
                i.size.map(|s| s.to_string()).unwrap_or_default(),
                i.status.code().to_string(),
                field(service),
                field(creator_id),
                field(&p.id),
                field(&p.title),
                field(date),
                field(&format!("https://pawchive.pw/{service}/user/{creator_id}/post/{}", p.id)),
                field(i.note.unwrap_or("")),
            ];
            out.push_str(&cols.join(","));
            out.push_str("\r\n");
        }
    }
    out.into_bytes()
}

/// Default file name: `patreon_5163822-links.csv`, or `patreon_5163822_post-162546844-links.csv`.
pub fn default_file_name(service: &str, creator_id: &str, post_id: Option<&str>, ext: &str) -> String {
    match post_id {
        Some(p) => format!("{service}_{creator_id}_post-{p}-links.{ext}"),
        None => format!("{service}_{creator_id}-links.{ext}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::{Host, LinkKind, PostRef};

    fn link(id: usize, url: &str, host: Host, kind: LinkKind, posts: &[(&str, &str, &str)]) -> Link {
        Link {
            id,
            url: url.into(),
            host,
            kind,
            name_hint: None,
            posts: posts
                .iter()
                .map(|(i, t, d)| PostRef { id: (*i).into(), title: (*t).into(), published: Some((*d).into()) })
                .collect(),
        }
    }

    fn sample() -> Vec<Link> {
        vec![
            link(
                0,
                "https://www.mediafire.com/file/k/Super_Girl.rar/file",
                Host::Mediafire,
                LinkKind::MediafireFile { key: "k".into(), page_url: "p".into() },
                &[("2", "=SUM(A1)", "2026-06-30T23:12:14"), ("1", "Plain, \"quoted\"", "2026-01-01T00:00:00")],
            ),
            link(
                1,
                "https://mega.nz/folder/Kp3a9QxZ#key",
                Host::Mega,
                LinkKind::Unsupported { why: "Mega isn't supported".into() },
                &[("2", "夏の日", "2026-04-02T01:00:00")],
            ),
        ]
    }

    #[test]
    fn txt_is_one_unique_url_per_line_with_no_bom() {
        let mut links = sample();
        links.push(links[0].clone());
        let t = to_txt(&links);
        assert_eq!(t, "https://www.mediafire.com/file/k/Super_Girl.rar/file\nhttps://mega.nz/folder/Kp3a9QxZ#key\n");
        assert!(!t.starts_with('\u{feff}') && !t.contains('\r'));
    }

    #[test]
    fn csv_has_bom_crlf_header_one_row_per_occurrence_and_defused_text() {
        let bytes = to_csv(&sample(), "patreon", "5163822", &|l| ExportInfo {
            status: if l.downloadable() { ExportStatus::NotChecked } else { ExportStatus::NotDownloadable },
            filename: l.downloadable().then(|| "Super_Girl.rar".to_string()),
            size: None,
            note: (!l.downloadable()).then_some("unsupported_host"),
        });
        assert_eq!(&bytes[..3], [0xEF, 0xBB, 0xBF]);
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.trim_start_matches('\u{feff}').split("\r\n").collect();
        assert_eq!(lines[0], CSV_HEADER);
        assert_eq!(lines.len(), 5, "header + 3 rows + trailing empty");
        assert_eq!(
            lines[1],
            "https://www.mediafire.com/file/k/Super_Girl.rar/file,Mediafire,yes,Super_Girl.rar,,not_checked,patreon,5163822,2,'=SUM(A1),2026-06-30,https://pawchive.pw/patreon/user/5163822/post/2,"
        );
        assert!(lines[2].contains(",1,\"Plain, \"\"quoted\"\"\",2026-01-01,"), "{}", lines[2]);
        assert_eq!(
            lines[3],
            "https://mega.nz/folder/Kp3a9QxZ#key,Mega,no,,,not_downloadable,patreon,5163822,2,夏の日,2026-04-02,https://pawchive.pw/patreon/user/5163822/post/2,unsupported_host"
        );
        assert_eq!(lines[4], "");
    }

    #[test]
    fn default_names() {
        assert_eq!(default_file_name("patreon", "5163822", None, "csv"), "patreon_5163822-links.csv");
        assert_eq!(default_file_name("patreon", "5163822", Some("162546844"), "txt"), "patreon_5163822_post-162546844-links.txt");
    }
}
