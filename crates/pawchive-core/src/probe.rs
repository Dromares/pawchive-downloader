//! Size / Content-Type probing with `Range: bytes=0-0`.

use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use reqwest::StatusCode;

use crate::control::Control;
use crate::error::Result;
use crate::gate::Gate;
use crate::http::{send_with_retry, Http, RetryPolicy};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeInfo {
    pub size: Option<u64>,
    pub content_type: Option<String>,
}

/// Total size from `Content-Range: bytes 0-0/12345` (`*` means unknown).
pub fn total_from_content_range(v: &str) -> Option<u64> {
    v.rsplit_once('/')?.1.trim().parse().ok()
}

/// One polite request for the total size and content type of `url`. A 404 is an error
/// (`Error::NotFound`); the response body is never read.
pub async fn probe(http: &Http, gate: &Gate, control: &Control, policy: &RetryPolicy, url: &str) -> Result<ProbeInfo> {
    let resp = send_with_retry(gate, control, policy, || http.get(url).header(RANGE, "bytes=0-0")).await?;
    let h = resp.headers();
    let size = if resp.status() == StatusCode::PARTIAL_CONTENT {
        h.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(total_from_content_range)
    } else {
        // 200: the server ignored the range and is describing the whole body.
        h.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
    };
    let content_type = h.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
    Ok(ProbeInfo { size, content_type })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_totals() {
        assert_eq!(total_from_content_range("bytes 0-0/199887803"), Some(199887803));
        assert_eq!(total_from_content_range("bytes 0-0/*"), None);
        assert_eq!(total_from_content_range("garbage"), None);
    }
}
