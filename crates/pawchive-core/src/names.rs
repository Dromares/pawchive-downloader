//! Filesystem-safe names: sanitising, byte caps, collision numbering, and the on-disk layout
//! `{output}/{service}_{creator}/{published-date}_{post_id}_{title}/{filename}`.

use std::collections::HashSet;

/// Longest single path component we produce, in bytes.
pub const MAX_COMPONENT_BYTES: usize = 200;

const ILLEGAL: &[char] = &['/', '\\', '<', '>', ':', '"', '|', '?', '*'];

fn is_reserved_stem(stem: &str) -> bool {
    let s = stem.to_ascii_uppercase();
    matches!(s.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (s.len() == 4
            && (s.starts_with("COM") || s.starts_with("LPT"))
            && s.as_bytes()[3].is_ascii_digit()
            && s.as_bytes()[3] != b'0')
}

fn trim_edges(s: &str) -> &str {
    s.trim_matches(|c: char| c == '.' || c.is_whitespace())
}

/// Truncate to at most `max` bytes on a UTF-8 boundary.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Split `name` into `(stem, ".ext")` where the extension is short and alphanumeric.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 17 && name[i + 1..].chars().all(|c| c.is_ascii_alphanumeric()) && i + 1 < name.len() => {
            (&name[..i], &name[i..])
        }
        _ => (name, ""),
    }
}

/// Make one path component safe on Linux, macOS and Windows.
///
/// Removes path separators, `<>:"|?*` and control characters, strips leading/trailing dots and
/// spaces, guards Windows reserved device names, caps the length at `max_bytes` (keeping a short
/// extension intact) and never returns an empty string.
pub fn sanitize_component(input: &str, max_bytes: usize) -> String {
    let cleaned: String = input.chars().filter(|c| !ILLEGAL.contains(c) && !c.is_control()).collect();
    let cleaned = trim_edges(&cleaned);
    if cleaned.is_empty() {
        return "_".to_string();
    }
    let (stem, ext) = split_ext(cleaned);
    let stem = if is_reserved_stem(stem) { format!("_{stem}") } else { stem.to_string() };
    let ext = if ext.len() < max_bytes { ext } else { "" };
    let stem = truncate_bytes(&stem, max_bytes.saturating_sub(ext.len()));
    let stem = trim_edges(stem);
    if stem.is_empty() {
        return format!("_{ext}");
    }
    format!("{stem}{ext}")
}

/// Filename inserted between stem and extension for thumbnails: `a.jpg` -> `a.thumb.jpg`.
pub fn thumb_name(name: &str) -> String {
    let (stem, ext) = split_ext(name);
    format!("{stem}.thumb{ext}")
}

/// Hands out unique names inside one folder: the second `a.jpg` becomes `a (2).jpg`, then
/// `a (3).jpg`. Comparison is case-insensitive so the result is safe on case-folding filesystems.
#[derive(Debug, Default)]
pub struct NameAllocator {
    taken: HashSet<String>,
}

impl NameAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allocate(&mut self, name: &str) -> String {
        let (stem, ext) = split_ext(name);
        let mut candidate = name.to_string();
        let mut n = 2u32;
        while !self.taken.insert(candidate.to_lowercase()) {
            let suffix = format!(" ({n})");
            let stem_cap = MAX_COMPONENT_BYTES.saturating_sub(ext.len() + suffix.len());
            candidate = format!("{}{suffix}{ext}", trim_edges(truncate_bytes(stem, stem_cap)));
            n += 1;
        }
        candidate
    }
}

/// `{service}_{creator}`
pub fn creator_dir(service: &str, creator_id: &str) -> String {
    sanitize_component(&format!("{service}_{creator_id}"), MAX_COMPONENT_BYTES)
}

/// `{published-date}_{post_id}_{title}`; the date is `YYYY-MM-DD` taken from `published`.
pub fn post_dir(published: Option<&str>, post_id: &str, title: &str) -> String {
    let date = published
        .and_then(|p| p.get(..10))
        .filter(|d| d.len() == 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-' && d.chars().all(|c| c.is_ascii_digit() || c == '-'))
        .unwrap_or("undated");
    sanitize_component(&format!("{date}_{post_id}_{}", title.trim()), MAX_COMPONENT_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_separators_and_illegal_characters() {
        assert_eq!(sanitize_component("a/b\\c<d>e:f\"g|h?i*j.png", 200), "abcdefghij.png");
        assert_eq!(sanitize_component("tab\tnew\nline\u{7}.txt", 200), "tabnewline.txt");
    }

    #[test]
    fn strips_leading_and_trailing_dots_and_spaces() {
        assert_eq!(sanitize_component("  ..hello world.. ", 200), "hello world");
        assert_eq!(sanitize_component("name. ", 200), "name");
        assert_eq!(sanitize_component("..", 200), "_");
        assert_eq!(sanitize_component("   ", 200), "_");
        assert_eq!(sanitize_component("", 200), "_");
        assert_eq!(sanitize_component("///", 200), "_");
    }

    #[test]
    fn path_traversal_cannot_survive() {
        let s = sanitize_component("../../etc/passwd", 200);
        assert!(!s.contains('/') && !s.starts_with('.'), "{s}");
        assert_eq!(s, "etcpasswd");
    }

    #[test]
    fn windows_reserved_names_are_guarded() {
        assert_eq!(sanitize_component("CON", 200), "_CON");
        assert_eq!(sanitize_component("nul.txt", 200), "_nul.txt");
        assert_eq!(sanitize_component("Com1.png", 200), "_Com1.png");
        assert_eq!(sanitize_component("lpt9", 200), "_lpt9");
        // not reserved
        assert_eq!(sanitize_component("COM0", 200), "COM0");
        assert_eq!(sanitize_component("console.txt", 200), "console.txt");
        assert_eq!(sanitize_component("COM10", 200), "COM10");
    }

    #[test]
    fn caps_length_on_a_utf8_boundary_and_keeps_the_extension() {
        let long = format!("{}.jpg", "あ".repeat(100)); // 300 bytes of stem
        let s = sanitize_component(&long, 200);
        assert!(s.len() <= 200, "{}", s.len());
        assert!(s.ends_with(".jpg"));
        assert!(s.chars().all(|c| c == 'あ' || ".jpg".contains(c)));
        let s2 = sanitize_component(&"é".repeat(150), 200);
        assert!(s2.len() <= 200 && s2.chars().all(|c| c == 'é'));
        // exactly at the limit is untouched
        let exact = "a".repeat(200);
        assert_eq!(sanitize_component(&exact, 200), exact);
    }

    #[test]
    fn unicode_names_pass_through() {
        assert_eq!(sanitize_component("暑がりブラウスJK.png", 200), "暑がりブラウスJK.png");
    }

    #[test]
    fn collisions_get_numbered_before_the_extension() {
        let mut a = NameAllocator::new();
        assert_eq!(a.allocate("pic.jpg"), "pic.jpg");
        assert_eq!(a.allocate("pic.jpg"), "pic (2).jpg");
        assert_eq!(a.allocate("pic.jpg"), "pic (3).jpg");
        assert_eq!(a.allocate("other.jpg"), "other.jpg");
        assert_eq!(a.allocate("noext"), "noext");
        assert_eq!(a.allocate("noext"), "noext (2)");
    }

    #[test]
    fn collisions_are_case_insensitive() {
        let mut a = NameAllocator::new();
        assert_eq!(a.allocate("Pic.JPG"), "Pic.JPG");
        assert_eq!(a.allocate("pic.jpg"), "pic (2).jpg");
    }

    #[test]
    fn a_literal_numbered_name_is_not_handed_out_twice() {
        let mut a = NameAllocator::new();
        assert_eq!(a.allocate("pic (2).jpg"), "pic (2).jpg");
        assert_eq!(a.allocate("pic.jpg"), "pic.jpg");
        assert_eq!(a.allocate("pic.jpg"), "pic (3).jpg");
    }

    #[test]
    fn collision_suffix_respects_the_length_cap() {
        let mut a = NameAllocator::new();
        let n = format!("{}.png", "x".repeat(196)); // 200 bytes
        assert_eq!(a.allocate(&n), n);
        let second = a.allocate(&n);
        assert!(second.len() <= 200, "{}", second.len());
        assert!(second.ends_with(" (2).png"));
    }

    #[test]
    fn thumb_names() {
        assert_eq!(thumb_name("a.jpg"), "a.thumb.jpg");
        assert_eq!(thumb_name("a.b.png"), "a.b.thumb.png");
        assert_eq!(thumb_name("noext"), "noext.thumb");
    }

    #[test]
    fn folder_layout() {
        assert_eq!(creator_dir("fanbox", "1386751"), "fanbox_1386751");
        assert_eq!(
            post_dir(Some("2023-03-10T07:25:56"), "5464553", "Ada: Titfuck?"),
            "2023-03-10_5464553_Ada Titfuck"
        );
        assert_eq!(post_dir(None, "1", "t"), "undated_1_t");
        assert_eq!(post_dir(Some("garbage"), "1", ""), "undated_1_");
        let long = post_dir(Some("2023-03-10T07:25:56"), "5464553", &"題".repeat(200));
        assert!(long.len() <= 200 && long.starts_with("2023-03-10_5464553_"));
    }
}
