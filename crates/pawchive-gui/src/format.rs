//! Small display helpers.

use std::time::Duration;

/// `1536` -> `"1.5 kB"`. SI units (1 MB = 1,000,000 B), one decimal above bytes.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// `4047` -> `"4,047"`.
pub fn comma(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1` -> `"1 file"`, `4047` -> `"4,047 files"`.
pub fn files(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{} files", comma(n))
    }
}

/// `74 s` -> `"1:14"`, `3725 s` -> `"1:02:05"`.
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// `"about 3 min left"`, from remaining seconds.
pub fn eta(secs: u64) -> String {
    match secs {
        0..=59 => "less than a minute left".into(),
        60..=5399 => format!("about {} min left", secs.div_ceil(60)),
        _ => format!("about {:.1} h left", secs as f64 / 3600.0),
    }
}

/// `HH:MM:SS` (UTC) for a unix timestamp.
pub fn hms_utc(unix_secs: u64) -> String {
    let d = unix_secs % 86_400;
    format!("{:02}:{:02}:{:02}", d / 3600, d / 60 % 60, d % 60)
}

/// Last path component, for "Files go in …/pawchive/fanbox_1386751/".
pub fn last_component(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// "2026-07-10_122215_暑がりブラ" -> "2026-07-10 · 暑がりブラ": the Post column shows date and title,
/// not the numeric post id. A name that is not `{date}_{id}_{title}` comes back unchanged, and a
/// folder with no title gives the date alone.
pub fn post_label(folder: &str) -> String {
    let mut parts = folder.splitn(3, '_');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(date), Some(id), title) if !date.is_empty() && !id.is_empty() => match title.map(str::trim) {
            Some(t) if !t.is_empty() => format!("{date} · {t}"),
            _ => date.to_string(),
        },
        _ => folder.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_labels() {
        assert_eq!(post_label("2026-07-10_122215_暑がりブラ"), "2026-07-10 · 暑がりブラ");
        assert_eq!(post_label("2026-07-10_122215_a_b"), "2026-07-10 · a_b");
        assert_eq!(post_label("2026-07-10_122215_"), "2026-07-10");
        assert_eq!(post_label("undated_7_t"), "undated · t");
        assert_eq!(post_label("loose"), "loose");
        assert_eq!(post_label(""), "");
    }

    #[test]
    fn file_counts_are_singular_for_one() {
        assert_eq!(files(0), "0 files");
        assert_eq!(files(1), "1 file");
        assert_eq!(files(4047), "4,047 files");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_500), "1.5 kB");
        assert_eq!(human_size(199_887_803), "199.9 MB");
        assert_eq!(human_size(1_234_000_000), "1.2 GB");
    }

    #[test]
    fn numbers_and_times() {
        assert_eq!(comma(0), "0");
        assert_eq!(comma(999), "999");
        assert_eq!(comma(4047), "4,047");
        assert_eq!(comma(1_234_567), "1,234,567");
        assert_eq!(clock(Duration::from_secs(14)), "0:14");
        assert_eq!(clock(Duration::from_secs(724)), "12:04");
        assert_eq!(clock(Duration::from_secs(3725)), "1:02:05");
        assert_eq!(eta(30), "less than a minute left");
        assert_eq!(eta(170), "about 3 min left");
        assert_eq!(hms_utc(3 * 3600 + 4 * 60 + 5), "03:04:05");
        assert_eq!(last_component("/home/you/Downloads/pawchive/"), "pawchive");
    }
}
