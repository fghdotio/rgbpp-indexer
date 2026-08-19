//! Politeness for public Bitcoin endpoints.
//!
//! Two independent limits: how many requests may be in flight at once, and how
//! closely together requests may start. Public endpoints enforce both, and getting
//! rate limited during a sweep is a slow, confusing failure mode.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct Throttle {
    permits: Arc<Semaphore>,
    min_interval: Duration,
    last_start: Arc<Mutex<Option<Instant>>>,
}

impl Throttle {
    pub fn new(max_concurrency: usize, min_interval_ms: u64) -> Self {
        Throttle {
            permits: Arc::new(Semaphore::new(max_concurrency.max(1))),
            min_interval: Duration::from_millis(min_interval_ms),
            last_start: Arc::new(Mutex::new(None)),
        }
    }

    pub fn unlimited() -> Self {
        Throttle::new(usize::from(u8::MAX), 0)
    }

    /// Wait until it is this caller's turn. The returned permit must be held for
    /// the duration of the request.
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("throttle semaphore is never closed");

        if !self.min_interval.is_zero() {
            let mut last = self.last_start.lock().await;
            let now = Instant::now();
            if let Some(previous) = *last {
                let elapsed = now.duration_since(previous);
                if elapsed < self.min_interval {
                    tokio::time::sleep(self.min_interval - elapsed).await;
                }
            }
            *last = Some(Instant::now());
        }
        permit
    }
}
