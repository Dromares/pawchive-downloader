//! A tiny in-process HTTP/1.1 server for tests. No network is involved beyond loopback.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;

#[derive(Clone, Debug)]
pub struct Req {
    pub path: String,
    /// Lower-cased header names.
    pub headers: HashMap<String, String>,
}

impl Req {
    pub fn range(&self) -> Option<&str> {
        self.headers.get("range").map(String::as_str)
    }
}

#[derive(Clone, Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Claim a longer Content-Length than the body, then hang up (a dropped connection).
    pub declared_len: Option<usize>,
    pub delay: Duration,
    /// Keep the connection open this long after the body bytes (a stalled transfer).
    pub stall: Duration,
}

impl Resp {
    pub fn status(status: u16) -> Self {
        Resp { status, headers: vec![], body: vec![], declared_len: None, delay: Duration::ZERO, stall: Duration::ZERO }
    }

    pub fn ok(body: &[u8]) -> Self {
        Resp { body: body.to_vec(), ..Resp::status(200) }
    }

    /// 206 for `body[start..]`, with a correct Content-Range.
    pub fn partial(body: &[u8], start: usize) -> Self {
        let mut r = Resp::status(206);
        r.headers.push(("Content-Range".into(), format!("bytes {start}-{}/{}", body.len() - 1, body.len())));
        r.body = body[start..].to_vec();
        r
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn delayed(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }

    pub fn stalled(mut self, d: Duration) -> Self {
        self.stall = d;
        self
    }

    pub fn truncated(mut self, declared: usize) -> Self {
        self.declared_len = Some(declared);
        self
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub at: Instant,
    pub req: Req,
}

pub struct Mock {
    pub base: String,
    pub hits: Arc<Mutex<Vec<Hit>>>,
    pub max_inflight: Arc<AtomicUsize>,
}

impl Mock {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
}

/// `handler(request, n)` where `n` is how many requests were seen before this one.
pub async fn serve(handler: impl Fn(&Req, usize) -> Resp + Send + Sync + 'static) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let hits = Arc::new(Mutex::new(Vec::new()));
    let inflight = Arc::new(AtomicUsize::new(0));
    let max_inflight = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(handler);
    let (h2, f2, m2) = (hits.clone(), inflight.clone(), max_inflight.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (handler, hits, inflight, max_inflight) = (handler.clone(), h2.clone(), f2.clone(), m2.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let mut lines = text.split("\r\n");
                let path = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("/").to_string();
                let headers: HashMap<String, String> = lines
                    .take_while(|l| !l.is_empty())
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let req = Req { path, headers };
                let n = {
                    let mut h = hits.lock().unwrap();
                    h.push(Hit { at: Instant::now(), req: req.clone() });
                    h.len() - 1
                };
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                max_inflight.fetch_max(now, Ordering::SeqCst);
                let resp = handler(&req, n);
                if !resp.delay.is_zero() {
                    tokio::time::sleep(resp.delay).await;
                }
                let mut head = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", resp.status);
                for (k, v) in &resp.headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str(&format!("Content-Length: {}\r\n\r\n", resp.declared_len.unwrap_or(resp.body.len())));
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&resp.body).await;
                let _ = sock.flush().await;
                if !resp.stall.is_zero() {
                    tokio::time::sleep(resp.stall).await;
                }
                inflight.fetch_sub(1, Ordering::SeqCst);
                // dropping the socket closes it
            });
        }
    });
    Mock { base: format!("http://{addr}"), hits, max_inflight }
}

/// Serve `body` at any path honouring `Range: bytes=N-` (206) like the real file host.
pub fn range_aware(body: Vec<u8>) -> impl Fn(&Req, usize) -> Resp + Send + Sync + 'static {
    move |req, _| match req.range().and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.strip_suffix('-')).and_then(|n| n.parse::<usize>().ok()) {
        Some(start) if start < body.len() => Resp::partial(&body, start),
        Some(_) => Resp::status(416),
        None => Resp::ok(&body),
    }
}
