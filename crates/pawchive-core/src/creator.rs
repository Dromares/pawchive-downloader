use url::Url;

use crate::error::{Error, Result};
use crate::model::Service;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreatorRef {
    pub service: Service,
    pub id: String,
}

/// Parse `https://pawchive.pw/{service}/user/{id}` (a trailing `/post/…`, query or fragment is
/// ignored). Unknown hosts and services are rejected with a message the UI can show directly.
pub fn parse_creator_url(input: &str) -> Result<CreatorRef> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::BadUrl("paste a Pawchive creator URL first".into()));
    }
    let with_scheme = if input.contains("://") { input.to_string() } else { format!("https://{input}") };
    let url = Url::parse(&with_scheme).map_err(|_| Error::BadUrl(format!("\"{input}\" is not a valid URL")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::BadUrl("only http(s) URLs are supported".into()));
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host != "pawchive.pw" && host != "www.pawchive.pw" {
        return Err(Error::BadUrl(format!(
            "\"{host}\" is not Pawchive; expected https://pawchive.pw/<service>/user/<id>"
        )));
    }
    let segs: Vec<&str> = url.path_segments().map(|s| s.filter(|x| !x.is_empty()).collect()).unwrap_or_default();
    match segs.as_slice() {
        [service, "user", id, ..] => {
            let service = Service::parse(service)?;
            if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(Error::BadUrl(format!("\"{id}\" is not a valid creator id")));
            }
            Ok(CreatorRef { service, id: (*id).to_string() })
        }
        _ => Err(Error::BadUrl("expected a creator URL like https://pawchive.pw/fanbox/user/1386751".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_decorated_urls() {
        let want = CreatorRef { service: Service::Fanbox, id: "1386751".into() };
        for u in [
            "https://pawchive.pw/fanbox/user/1386751",
            "  https://pawchive.pw/fanbox/user/1386751/ ",
            "http://www.pawchive.pw/Fanbox/user/1386751?o=50#x",
            "pawchive.pw/fanbox/user/1386751/post/5464553",
        ] {
            assert_eq!(parse_creator_url(u).unwrap(), want, "{u}");
        }
    }

    #[test]
    fn rejects_bad_input_with_clear_messages() {
        for (u, frag) in [
            ("", "paste"),
            ("https://example.com/fanbox/user/1", "not Pawchive"),
            ("https://pawchive.pw/pixiv/user/1", "unsupported service"),
            ("https://pawchive.pw/fanbox/creators", "expected a creator URL"),
            ("https://pawchive.pw/fanbox/user/a b", "valid"),
            ("ftp://pawchive.pw/fanbox/user/1", "http"),
        ] {
            let e = parse_creator_url(u).unwrap_err().to_string();
            assert!(e.contains(frag), "{u}: {e}");
        }
    }
}
