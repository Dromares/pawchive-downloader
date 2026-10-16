//! File type classification: extension first (from the name, else the path), then the
//! Content-Type seen by the size probe when the extension is missing or contradicted.

use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Image,
    Archive,
    Text,
    Video,
    Other,
    /// Matched the user's custom-extensions field (takes precedence over everything else).
    Custom,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Image,
        Category::Archive,
        Category::Text,
        Category::Video,
        Category::Other,
        Category::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Image => "Images",
            Category::Archive => "Archives",
            Category::Text => "Text",
            Category::Video => "Video",
            Category::Other => "Other",
            Category::Custom => "Custom extensions",
        }
    }
}

/// Lower-cased extension of `s` (the part after the last `.`), if it looks like one.
pub fn extension_of(s: &str) -> Option<String> {
    let leaf = s.rsplit(['/', '\\']).next().unwrap_or(s);
    let (stem, ext) = leaf.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 10 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

fn category_for_extension(ext: &str) -> Category {
    match ext {
        "jpg" | "jpeg" | "png" | "gif" | "webp" => Category::Image,
        "zip" | "rar" | "7z" => Category::Archive,
        "txt" | "md" => Category::Text,
        "mp4" | "webm" | "mov" | "mkv" | "avi" => Category::Video,
        _ => Category::Other,
    }
}

/// `None` for types that say nothing useful (`application/octet-stream`, `text/html`, …).
fn category_for_content_type(ct: &str) -> Option<Category> {
    let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match ct.as_str() {
        "text/plain" | "text/markdown" => Some(Category::Text),
        "application/zip"
        | "application/x-zip-compressed"
        | "application/vnd.rar"
        | "application/x-rar-compressed"
        | "application/x-rar"
        | "application/x-7z-compressed" => Some(Category::Archive),
        _ if ct.starts_with("image/") => Some(Category::Image),
        _ if ct.starts_with("video/") => Some(Category::Video),
        _ => None,
    }
}

/// User-supplied extensions, e.g. `psd, .MP4;clip`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustomExts(HashSet<String>);

impl CustomExts {
    pub fn parse(input: &str) -> Self {
        CustomExts(
            input
                .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
                .map(|t| t.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .collect(),
        )
    }

    pub fn contains(&self, ext: &str) -> bool {
        self.0.contains(ext)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub fn classify(name: &str, path: &str, custom: &CustomExts, content_type: Option<&str>) -> Category {
    let ext = extension_of(name).or_else(|| extension_of(path));
    if let Some(e) = &ext {
        if custom.contains(e) {
            return Category::Custom;
        }
    }
    let from_ext = ext.as_deref().map(category_for_extension);
    let from_ct = content_type.and_then(category_for_content_type);
    match (from_ext, from_ct) {
        // No usable extension: the Content-Type decides, otherwise it is "other".
        (None, ct) => ct.unwrap_or(Category::Other),
        // Extension and Content-Type disagree: trust what the server actually sent.
        (Some(e), Some(c)) if e != c => c,
        (Some(e), _) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> CustomExts {
        CustomExts::default()
    }

    #[test]
    fn extensions_by_category() {
        for (n, c) in [
            ("a.jpg", Category::Image),
            ("a.JPEG", Category::Image),
            ("a.png", Category::Image),
            ("a.gif", Category::Image),
            ("a.webp", Category::Image),
            ("a.zip", Category::Archive),
            ("a.rar", Category::Archive),
            ("a.7z", Category::Archive),
            ("a.txt", Category::Text),
            ("a.md", Category::Text),
            ("a.mp4", Category::Video),
            ("a.webm", Category::Video),
            ("a.mov", Category::Video),
            ("a.mkv", Category::Video),
            ("a.avi", Category::Video),
            ("a.psd", Category::Other),
            ("a.tar.gz", Category::Other),
        ] {
            assert_eq!(classify(n, "/x/y/hash.bin", &none(), None), c, "{n}");
        }
    }

    #[test]
    fn falls_back_to_the_path_extension_when_the_name_has_none() {
        assert_eq!(classify("cover", "/ab/cd/ef.png", &none(), None), Category::Image);
        assert_eq!(classify("README", "/ab/cd/ef", &none(), None), Category::Other);
        assert_eq!(classify(".hidden", "/ab/cd/ef.zip", &none(), None), Category::Archive);
    }

    #[test]
    fn custom_extensions_take_precedence() {
        let c = CustomExts::parse("psd, .MP4;clip");
        assert_eq!(classify("a.psd", "/p", &c, None), Category::Custom);
        assert_eq!(classify("a.mp4", "/p", &c, None), Category::Custom);
        assert_eq!(classify("a.clip", "/p", &c, None), Category::Custom);
        assert_eq!(classify("a.mkv", "/p", &c, None), Category::Video);
        // even a contradicting Content-Type does not take it back
        assert_eq!(classify("a.psd", "/p", &c, Some("image/png")), Category::Custom);
        assert!(CustomExts::parse(" , ;").is_empty());
    }

    #[test]
    fn content_type_fills_in_a_missing_extension() {
        assert_eq!(classify("data", "/ab/cd/ef", &none(), Some("image/jpeg")), Category::Image);
        assert_eq!(classify("data", "/ab/cd/ef", &none(), Some("video/mp4")), Category::Video);
        assert_eq!(classify("data", "/ab/cd/ef", &none(), Some("application/zip")), Category::Archive);
        assert_eq!(classify("data", "/ab/cd/ef", &none(), Some("text/plain; charset=utf-8")), Category::Text);
        assert_eq!(classify("data", "/ab/cd/ef", &none(), Some("application/octet-stream")), Category::Other);
    }

    #[test]
    fn content_type_overrides_a_contradicted_extension() {
        // named .jpg, served as a zip
        assert_eq!(classify("a.jpg", "/p", &none(), Some("application/zip")), Category::Archive);
        // unknown extension, served as an image
        assert_eq!(classify("a.bin", "/p", &none(), Some("image/png")), Category::Image);
        assert_eq!(classify("a.zip", "/p", &none(), Some("Application/ZIP")), Category::Archive);
    }

    #[test]
    fn uninformative_content_types_do_not_override() {
        for ct in ["application/octet-stream", "text/html", "binary/octet-stream", ""] {
            assert_eq!(classify("a.png", "/p", &none(), Some(ct)), Category::Image, "{ct}");
        }
    }

    #[test]
    fn extension_parsing_edge_cases() {
        assert_eq!(extension_of("a.b.JPG").as_deref(), Some("jpg"));
        assert_eq!(extension_of("noext"), None);
        assert_eq!(extension_of(".bashrc"), None);
        assert_eq!(extension_of("trailing."), None);
        assert_eq!(extension_of("weird.ext name"), None);
        assert_eq!(extension_of("dir.d/file"), None);
    }
}
