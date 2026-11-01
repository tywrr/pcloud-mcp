//! Whether the storage backend works, as the last health check (or failed call) found it.
//!
//! The server checks every few minutes and right after a call fails at the backend; index
//! tools keep answering from the index and say since when the backend has been failing, and
//! live calls fail fast with the same message instead of waiting on a dead backend.

use std::sync::Mutex;

use tokio::sync::Notify;

use crate::time;

/// Since when the backend has been failing, and the last error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unhealthy {
    pub since: i64,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct Health {
    state: Mutex<Option<Unhealthy>>,
    poke: Notify,
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// `None` while the backend works.
    pub fn status(&self) -> Option<Unhealthy> {
        self.lock().clone()
    }

    pub fn is_healthy(&self) -> bool {
        self.lock().is_none()
    }

    pub fn set_ok(&self) {
        *self.lock() = None;
    }

    /// Records a failure; `since` stays at the first failure of a run of them.
    pub fn set_failed(&self, error: &str) {
        let mut state = self.lock();
        let since = state.as_ref().map_or_else(time::now, |s| s.since);
        *state = Some(Unhealthy {
            since,
            error: error.chars().take(300).collect(),
        });
    }

    /// Asks the health checker to check now (a call just failed at the backend).
    pub fn poke(&self) {
        self.poke.notify_one();
    }

    /// Resolves at the next [`Health::poke`], for the checker's loop.
    pub async fn poked(&self) {
        self.poke.notified().await;
    }

    /// `backend unhealthy since 13:40Z: <error>`, when it is.
    pub fn describe(&self) -> Option<String> {
        self.status().map(|s| {
            format!(
                "backend unhealthy since {}: {}",
                time::short(s.since, time::now()),
                s.error
            )
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Unhealthy>> {
        // A panic while holding this lock can't leave the state half-written.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_is_the_first_failure() {
        let health = Health::new();
        assert!(health.is_healthy() && health.describe().is_none());
        health.set_failed("token expired");
        let first = health.status().unwrap().since;
        health.set_failed("still expired");
        let state = health.status().unwrap();
        assert_eq!(
            (state.since, state.error.as_str()),
            (first, "still expired")
        );
        assert!(health.describe().unwrap().contains("still expired"));
        health.set_ok();
        assert!(health.is_healthy());
    }
}
