//! Stop tokens for live external (process) children.
//!
//! An in-process child is reachable through its runner handle; a process
//! child is not, so the fleet's stop needs a place to find it. One map of
//! child id to stop token is that place, and keeping it here keeps the
//! registry and its three operations out of the runtime's own field list.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use houyicoder_async::CancellationToken;

/// The live external children of one runtime, keyed by child id.
#[derive(Clone, Default)]
pub(crate) struct ExternalChildren {
    live: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl ExternalChildren {
    /// File a child's stop token once its process exists.
    pub(super) fn register(&self, child_id: String, stop: CancellationToken) {
        if let Ok(mut live) = self.live.lock() {
            live.insert(child_id, stop);
        }
    }

    /// Drop a child's stop token once its process is gone.
    pub(super) fn unregister(&self, child_id: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(child_id);
        }
    }

    /// End a process child by id. True when a token was found and cancelled,
    /// which is how a fleet stop reaches a child with no runner handle.
    pub(super) fn stop(&self, child_id: &str) -> bool {
        let stop = self
            .live
            .lock()
            .ok()
            .and_then(|live| live.get(child_id).cloned());
        match stop {
            Some(stop) => {
                stop.cancel();
                true
            }
            None => false,
        }
    }
}
