//! HTTP plumbing: the shared client (user agent, optional cookies) and the retry policy used by
//! API calls, size probes and downloads.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, COOKIE, RETRY_AFTER};
use reqwest::{Client, RequestBuilder, Response, StatusCode};

use crate::control::Control;
use crate::cookies::CookieJar;
use crate::error::{Error, Result};
use crate::gate::Gate;
use crate::settings::Settings;

#[derive(Clone, Debug)]
pub struct Http {
    client: Client,
    cookies: Option<Arc<CookieJar>>,
}

impl Http {
    pub fn new(settings: &Settings, cookies: Option<CookieJar>) -> Result<Self> {
        let client = Client::builder()
            .user_agent(settings.user_agent())
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(Error::from_reqwest)?;
        Ok(Http { client, cookies: cookies.filter(|c| !c.is_empty()).map(Arc::new) })
    }

    pub fn get(&self, url: &str) -> RequestBuilder {
        let mut rb = self.client.get(url);
        if let (Some(jar), Ok(parsed)) = (&self.cookies, url::Url::parse(url)) {
            if let Some(h) = jar.header_for(&parsed) {
                // Marked sensitive so it is redacted from reqwest/hyper debug output.
                if let Ok(mut v) = reqwest::header::HeaderValue::from_str(&h) {
                    v.set_sensitive(true);
                    rb = rb.header(COOKIE, v);
                }
            }
        }
        rb
    }
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Total requests per item, including the first.
    pub max_tries: u32,
    /// First backoff; doubles each retry (2 s, 4 s, 8 s, …).
    pub base: Duration,
    pub cap: Duration,
    /// How long all requests stop after a 403 bot page.
    pub forbidden_pause: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_tries: 5,
            base: Duration::from_secs(2),
            cap: Duration::from_secs(120),
            forbidden_pause: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// Wait before retry number `attempt` (1-based), honouring `Retry-After` when it is longer.
    pub fn backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exp = self.base.saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
        exp.max(retry_after.unwrap_or_default()).min(self.cap)
    }
}

/// `Retry-After` in delta-seconds form. (The HTTP-date form is ignored; backoff covers it.)
pub fn retry_after(h: &HeaderMap) -> Option<Duration> {
    h.get(RETRY_AFTER)?.to_str().ok()?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// 404 and other non-retryable client errors.
    Permanent,
    /// 403 bot page: pause everything, then retry.
    BotBlock,
    /// 429, 5xx.
    Retry,
}

pub fn classify_status(s: StatusCode) -> Verdict {
    match s.as_u16() {
        200..=299 => Verdict::Ok,
        403 => Verdict::BotBlock,
        429 | 500..=599 => Verdict::Retry,
        _ => Verdict::Permanent,
    }
}

/// Send a small request (API page, size probe) through the gate with the shared retry policy.
/// `build` is called for each attempt. 404 and other 4xx become errors immediately.
pub async fn send_with_retry(
    gate: &Gate,
    control: &Control,
    policy: &RetryPolicy,
    mut build: impl FnMut() -> RequestBuilder,
) -> Result<Response> {
    let mut attempt = 0u32;
    loop {
        if !gate.acquire(control).await {
            return Err(Error::Cancelled);
        }
        attempt += 1;
        let (reason, wait) = match build().send().await {
            Ok(resp) => {
                let status = resp.status();
                match classify_status(status) {
                    Verdict::Ok => return Ok(resp),
                    Verdict::Permanent if status == StatusCode::NOT_FOUND => {
                        return Err(Error::NotFound(resp.url().path().to_string()));
                    }
                    Verdict::Permanent => {
                        return Err(Error::Status { status: status.as_u16(), url: resp.url().path().to_string() });
                    }
                    Verdict::BotBlock => {
                        gate.pause_for(policy.forbidden_pause);
                        (
                            format!("HTTP 403 (bot page), pausing all requests {}s", policy.forbidden_pause.as_secs()),
                            Duration::ZERO,
                        )
                    }
                    Verdict::Retry => {
                        let ra = retry_after(resp.headers());
                        (format!("HTTP {}", status.as_u16()), policy.backoff(attempt, ra))
                    }
                }
            }
            Err(e) => (Error::from_reqwest(e).to_string(), policy.backoff(attempt, None)),
        };
        if attempt >= policy.max_tries {
            return Err(Error::Http(format!("gave up after {attempt} tries: {reason}")));
        }
        if !control.sleep(wait).await {
            return Err(Error::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_honours_retry_after() {
        let p = RetryPolicy::default();
        let secs = |a, ra: Option<u64>| p.backoff(a, ra.map(Duration::from_secs)).as_secs();
        assert_eq!([secs(1, None), secs(2, None), secs(3, None), secs(4, None)], [2, 4, 8, 16]);
        assert_eq!(secs(1, Some(30)), 30);
        assert_eq!(secs(3, Some(1)), 8);
        assert_eq!(secs(20, None), 120); // capped
        assert_eq!(secs(1, Some(100_000)), 120);
    }

    #[test]
    fn status_verdicts() {
        use Verdict::*;
        for (code, v) in [
            (200, Ok),
            (206, Ok),
            (403, BotBlock),
            (404, Permanent),
            (410, Permanent),
            (429, Retry),
            (500, Retry),
            (503, Retry),
            (400, Permanent),
        ] {
            assert_eq!(classify_status(StatusCode::from_u16(code).unwrap()), v, "{code}");
        }
    }
}
