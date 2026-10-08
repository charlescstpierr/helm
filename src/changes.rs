//! "Something on the board changed": the one notification both the HTTP handlers and the
//! supervisor publish, and the SSE stream relays.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Changes {
    /// Carries the board revision after each change; subscribers just reload.
    events: broadcast::Sender<u64>,
    revision: Arc<AtomicU64>,
}

impl Changes {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            events,
            revision: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn publish(&self) {
        let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        // No subscriber simply means no browser tab is open.
        let _ = self.events.send(revision);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<u64> {
        self.events.subscribe()
    }
}
