//! Cooperative shutdown.
//!
//! Background workers sleep between rounds. Sleeping on a shutdown signal instead of
//! a bare timer is what keeps `Ctrl-C` from waiting out a 24-hour sweep interval.

use std::time::Duration;

use tokio::sync::watch;

#[derive(Debug)]
pub struct ShutdownController {
    tx: watch::Sender<bool>,
}

#[derive(Clone, Debug)]
pub struct Shutdown {
    rx: watch::Receiver<bool>,
}

impl Default for ShutdownController {
    fn default() -> Self {
        Self::new()
    }
}

impl ShutdownController {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        ShutdownController { tx }
    }

    pub fn subscribe(&self) -> Shutdown {
        Shutdown {
            rx: self.tx.subscribe(),
        }
    }

    pub fn trigger(&self) {
        let _ = self.tx.send(true);
    }
}

impl Shutdown {
    pub fn is_triggered(&self) -> bool {
        *self.rx.borrow()
    }

    /// Sleep, returning `true` if shutdown was signalled instead of the timer firing.
    pub async fn sleep(&mut self, duration: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(duration) => self.is_triggered(),
            _ = self.rx.changed() => true,
        }
    }

    pub async fn wait(&mut self) {
        while !self.is_triggered() {
            if self.rx.changed().await.is_err() {
                return;
            }
        }
    }
}
