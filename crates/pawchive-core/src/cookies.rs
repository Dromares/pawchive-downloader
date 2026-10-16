//! Optional Netscape `cookies.txt` support. The file is read into memory once; cookie values are
//! never logged, displayed, or written anywhere (`Debug` prints counts only).

use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use url::Url;

use crate::error::{Error, Result};

#[derive(Clone)]
struct Cookie {
    domain: String,
    include_subdomains: bool,
    path: String,
    secure: bool,
    expires: u64, // 0 = session cookie
    name: String,
    value: String,
}

#[derive(Clone, Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CookieJar({} cookies, values hidden)", self.cookies.len())
    }
}

impl CookieJar {
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Read a cookies.txt file into memory. Errors never include file contents.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Cookies { path: path.to_path_buf(), msg: e.to_string() })?;
        Ok(Self::parse(&text))
    }

    /// Lenient parse: comment, blank and malformed lines are skipped. `#HttpOnly_` lines count.
    pub fn parse(text: &str) -> Self {
        let mut cookies = Vec::new();
        for raw in text.lines() {
            let line = raw.trim_end_matches('\r');
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 7 {
                continue;
            }
            cookies.push(Cookie {
                domain: f[0].trim_start_matches('.').to_ascii_lowercase(),
                include_subdomains: f[1].eq_ignore_ascii_case("TRUE"),
                path: f[2].to_string(),
                secure: f[3].eq_ignore_ascii_case("TRUE"),
                expires: f[4].parse().unwrap_or(0),
                name: f[5].to_string(),
                value: f[6].to_string(),
            });
        }
        CookieJar { cookies }
    }

    /// The `Cookie:` header value for `url`, or `None` when nothing applies.
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.header_at(url, now)
    }

    fn header_at(&self, url: &Url, now: u64) -> Option<String> {
        let host = url.host_str()?.to_ascii_lowercase();
        let path = url.path();
        let parts: Vec<String> = self
            .cookies
            .iter()
            .filter(|c| {
                let domain_ok = host == c.domain || (c.include_subdomains && host.ends_with(&format!(".{}", c.domain)));
                let path_ok = path.starts_with(c.path.trim_end_matches('/')) || c.path == "/";
                let secure_ok = !c.secure || url.scheme() == "https";
                let live = c.expires == 0 || c.expires > now;
                domain_ok && path_ok && secure_ok && live
            })
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Obviously fake values.
    const SAMPLE: &str = "# Netscape HTTP Cookie File\n\
.pawchive.pw\tTRUE\t/\tTRUE\t0\tsession\tFAKE-SESSION-VALUE\n\
#HttpOnly_pawchive.pw\tFALSE\t/\tFALSE\t4102444800\thttponly\tFAKE-H\n\
.other.example\tTRUE\t/\tFALSE\t0\tnope\tFAKE-O\n\
.pawchive.pw\tTRUE\t/\tFALSE\t1\texpired\tFAKE-E\n\
.pawchive.pw\tTRUE\t/api\tFALSE\t0\tapi_only\tFAKE-A\n\
garbage line\n\n";

    #[test]
    fn parses_and_matches_by_domain_path_secure_and_expiry() {
        let jar = CookieJar::parse(SAMPLE);
        assert_eq!(jar.len(), 5);
        let h = jar.header_at(&Url::parse("https://file.pawchive.pw/data/x").unwrap(), 1000).unwrap();
        assert_eq!(h, "session=FAKE-SESSION-VALUE"); // subdomain ok, expired + other-domain + /api out
        let h = jar.header_at(&Url::parse("https://pawchive.pw/api/v1/x").unwrap(), 1000).unwrap();
        assert_eq!(h, "session=FAKE-SESSION-VALUE; httponly=FAKE-H; api_only=FAKE-A");
        // secure cookie is not sent over http; host-only cookie not sent to a subdomain
        let h = jar.header_at(&Url::parse("http://pawchive.pw/").unwrap(), 1000).unwrap();
        assert_eq!(h, "httponly=FAKE-H");
        let h = jar.header_at(&Url::parse("https://img.pawchive.pw/").unwrap(), 1000).unwrap();
        assert!(!h.contains("httponly"));
        assert!(jar.header_at(&Url::parse("https://example.com/").unwrap(), 1000).is_none());
    }

    #[test]
    fn debug_never_prints_values() {
        let jar = CookieJar::parse(SAMPLE);
        let s = format!("{jar:?}");
        assert!(!s.contains("FAKE") && s.contains("5 cookies"), "{s}");
    }

    #[test]
    fn load_error_does_not_include_contents() {
        let e = CookieJar::load(Path::new("/nonexistent/cookies.txt")).unwrap_err().to_string();
        assert!(e.contains("/nonexistent/cookies.txt"));
    }
}
