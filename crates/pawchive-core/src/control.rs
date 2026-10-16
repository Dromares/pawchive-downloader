//! Pause / resume / cancel signalling shared by the scan, probe and download tasks.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Running,
    Paused,
    Cancelled,
}

/// A cloneable handle. All clones observe the same state.
#[derive(Clone, Debug)]
pub struct Control {
    tx: Arc<watch::Sender<RunState>>,
}

impl Default for Control {
    fn default() -> Self {
        Self::new()
    }
}

impl Control {
    pub fn new() -> Self {
        Control { tx: Arc::new(watch::channel(RunState::Running).0) }
    }

    pub fn state(&self) -> RunState {
        *self.tx.borrow()
    }

    pub fn is_cancelled(&self) -> bool {
        self.state() == RunState::Cancelled
    }

    pub fn pause(&self) {
        self.tx.send_if_modified(|s| {
            if *s == RunState::Running {
                *s = RunState::Paused;
                true
            } else {
                false
            }
        });
    }

    pub fn resume(&self) {
        self.tx.send_if_modified(|s| {
            if *s == RunState::Paused {
                *s = RunState::Running;
                true
            } else {
                false
            }
        });
    }

    pub fn cancel(&self) {
        self.tx.send_replace(RunState::Cancelled);
    }

    /// Wait while paused. Returns `false` if the run was cancelled, `true` once running.
    pub async fn wait_while_paused(&self) -> bool {
        let mut rx = self.tx.subscribe();
        loop {
            match *rx.borrow_and_update() {
                RunState::Running => return true,
                RunState::Cancelled => return false,
                RunState::Paused => {}
            }
            if rx.changed().await.is_err() {
                return false;
            }
        }
    }

    /// Resolves once the run is cancelled.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        loop {
            if *rx.borrow_and_update() == RunState::Cancelled {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Resolves as soon as the state is anything other than `Running` (paused or cancelled).
    pub async fn interrupted(&self) {
        let mut rx = self.tx.subscribe();
        loop {
            if *rx.borrow_and_update() != RunState::Running {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Sleep for `d`; returns `false` if cancelled first. Pausing does not interrupt a sleep.
    pub async fn sleep(&self, d: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(d) => true,
            _ = self.cancelled() => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pause_blocks_until_resume() {
        let c = Control::new();
        c.pause();
        let c2 = c.clone();
        let h = tokio::spawn(async move { c2.wait_while_paused().await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!h.is_finished());
        c.resume();
        assert!(h.await.unwrap());
    }

    #[tokio::test]
    async fn cancel_wins_over_pause() {
        let c = Control::new();
        c.pause();
        let c2 = c.clone();
        let h = tokio::spawn(async move { c2.wait_while_paused().await });
        c.cancel();
        assert!(!h.await.unwrap());
        // resume must not un-cancel
        c.resume();
        assert!(c.is_cancelled());
        assert!(!c.sleep(Duration::from_secs(5)).await);
    }
}
