//! Global request pacing. Every request start (API page, size probe, download attempt) passes
//! through one `Gate`, so the "at most one request per second" rule holds across all hosts.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

use crate::control::Control;

#[derive(Clone, Debug)]
pub struct Gate {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    interval: Duration,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    next_slot: Instant,
    paused_until: Option<Instant>,
}

impl Gate {
    pub fn new(interval: Duration) -> Self {
        Gate {
            inner: Arc::new(Inner {
                interval,
                state: Mutex::new(State {
                    next_slot: Instant::now(),
                    paused_until: None,
                }),
            }),
        }
    }

    pub fn interval(&self) -> Duration {
        self.inner.interval
    }

    /// Block every request for `d` (a 403 bot page is a global rate signal). Extends, never shortens.
    pub fn pause_for(&self, d: Duration) {
        let until = Instant::now() + d;
        let mut st = self.inner.state.lock().unwrap();
        st.paused_until = Some(st.paused_until.map_or(until, |p| p.max(until)));
    }

    /// Wait for the next request slot. Returns `false` if `control` was cancelled while waiting.
    pub async fn acquire(&self, control: &Control) -> bool {
        loop {
            if control.is_cancelled() {
                return false;
            }
            let wait = {
                let mut st = self.inner.state.lock().unwrap();
                let now = Instant::now();
                match st.paused_until {
                    Some(p) if p > now => p - now,
                    _ if st.next_slot > now => st.next_slot - now,
                    _ => {
                        st.paused_until = None;
                        st.next_slot = now + self.inner.interval;
                        return true;
                    }
                }
            };
            if !control.sleep(wait).await {
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn slots_are_spaced_by_the_interval() {
        let gate = Gate::new(Duration::from_secs(1));
        let c = Control::new();
        let t0 = Instant::now();
        let mut starts = vec![];
        for _ in 0..4 {
            assert!(gate.acquire(&c).await);
            starts.push(Instant::now() - t0);
        }
        for w in starts.windows(2) {
            assert!(w[1] - w[0] >= Duration::from_secs(1), "{starts:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_share_one_budget() {
        let gate = Gate::new(Duration::from_secs(1));
        let c = Control::new();
        let t0 = Instant::now();
        let mut hs = vec![];
        for _ in 0..5 {
            let (g, c) = (gate.clone(), c.clone());
            hs.push(tokio::spawn(async move {
                g.acquire(&c).await;
                Instant::now() - t0
            }));
        }
        let mut times = vec![];
        for h in hs {
            times.push(h.await.unwrap());
        }
        times.sort();
        for w in times.windows(2) {
            assert!(w[1] - w[0] >= Duration::from_secs(1), "{times:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pause_blocks_all_requests() {
        let gate = Gate::new(Duration::from_millis(10));
        let c = Control::new();
        gate.pause_for(Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(gate.acquire(&c).await);
        assert!(Instant::now() - t0 >= Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_releases_waiters() {
        let gate = Gate::new(Duration::from_secs(1));
        let c = Control::new();
        gate.pause_for(Duration::from_secs(3600));
        let (g, c2) = (gate.clone(), c.clone());
        let h = tokio::spawn(async move { g.acquire(&c2).await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        c.cancel();
        assert!(!h.await.unwrap());
    }
}
