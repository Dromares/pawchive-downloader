//! Profile-link check with the exact inline messages from `ui-spec` section 8. The parsing itself
//! is core's `parse_scan_url`; this only chooses which message to show.

use pawchive_core::creator::{parse_scan_url, ScanTarget};
use pawchive_core::model::Service;

const NO_ID: &str = "That link has no creator id. Use https://pawchive.pw/{service}/user/{id}.";

pub fn check(input: &str) -> Result<ScanTarget, String> {
    let t = input.trim();
    if t.is_empty() || t.chars().any(char::is_whitespace) || (!t.contains("://") && !t.contains('.')) {
        return Err("That doesn't look like a link. Paste a profile link from pawchive.pw.".into());
    }
    let rest = t.split_once("://").map_or(t, |(_, r)| r);
    let authority_and_path = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = authority_and_path.split_once('/').unwrap_or((authority_and_path, ""));
    let host = authority.rsplit('@').next().unwrap_or("").split(':').next().unwrap_or("").to_ascii_lowercase();
    if host != "pawchive.pw" && host != "www.pawchive.pw" {
        return Err("This isn't a Pawchive link. Use https://pawchive.pw/{service}/user/{id}.".into());
    }
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    match segs.next() {
        None => return Err(NO_ID.into()),
        Some(service) if Service::parse(service).is_err() => {
            return Err(format!(
                "Unknown service \u{201c}{service}\u{201d}. Pawchive Downloader supports patreon and fanbox."
            ))
        }
        Some(_) => {}
    }
    parse_scan_url(t).map_err(|_| NO_ID.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_forms_the_spec_forgives() {
        for u in [
            "https://pawchive.pw/fanbox/user/1386751",
            "  http://pawchive.pw/fanbox/user/1386751/  ",
            "https://pawchive.pw/fanbox/user/1386751?o=50",
            "https://www.pawchive.pw/Fanbox/user/1386751#top",
        ] {
            let t = check(u).unwrap_or_else(|e| panic!("{u}: {e}"));
            assert_eq!(
                (t.creator.service, t.creator.id.as_str(), t.post_id),
                (Service::Fanbox, "1386751", None),
                "{u}"
            );
        }
    }

    #[test]
    fn a_post_link_keeps_its_post_id() {
        let t = check("https://pawchive.pw/patreon/user/5163822/post/162546844").unwrap();
        assert_eq!((t.creator.id.as_str(), t.post_id.as_deref()), ("5163822", Some("162546844")));
    }

    #[test]
    fn four_error_messages_are_the_spec_copy() {
        let not_pawchive = "This isn't a Pawchive link. Use https://pawchive.pw/{service}/user/{id}.";
        assert_eq!(check("https://example.com/fanbox/user/1").unwrap_err(), not_pawchive);
        assert_eq!(check("example.com").unwrap_err(), not_pawchive);
        assert_eq!(
            check("https://pawchive.pw/foo/user/1").unwrap_err(),
            "Unknown service \u{201c}foo\u{201d}. Pawchive Downloader supports patreon and fanbox."
        );
        let no_id = "That link has no creator id. Use https://pawchive.pw/{service}/user/{id}.";
        for u in [
            "https://pawchive.pw/",
            "https://pawchive.pw/fanbox",
            "https://pawchive.pw/fanbox/user/",
            "https://pawchive.pw/fanbox/user/a b",
        ] {
            let e = check(u).unwrap_err();
            // "a b" has a space, so it is "not a link at all"; the others are missing ids
            assert!(e == no_id || u.ends_with("a b"), "{u}: {e}");
        }
        let not_a_url = "That doesn't look like a link. Paste a profile link from pawchive.pw.";
        for u in ["banana", "hello world", ""] {
            assert_eq!(check(u).unwrap_err(), not_a_url, "{u:?}");
        }
    }

    #[test]
    fn a_host_that_only_looks_like_pawchive_is_rejected() {
        assert!(check("https://pawchive.pw.evil.example/fanbox/user/1").unwrap_err().starts_with("This isn't"));
        assert!(check("https://evil.example/@pawchive.pw/fanbox/user/1").unwrap_err().starts_with("This isn't"));
    }
}
