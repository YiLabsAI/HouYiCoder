//! Consent persistence. When the user approves a tool invocation, the
//! approval is stored against the tool name plus an args key, with a TTL.
//! Future calls with the same key auto-allow until the entry expires, so a
//! repeated edit loop does not re-prompt on every step.
//!
//! The store is an abstract trait so a file-backed or session-scoped impl can
//! drop in later; the in-memory impl here is the default and is process-scoped.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A consent store records and recalls user approvals keyed by tool + args.
/// recall returns true only if a matching entry exists and has not exceeded
/// its TTL. record stores a fresh entry (restarting the TTL clock).
pub trait ConsentStore: Send + Sync {
    fn recall(&self, tool: &str, args_key: &str) -> bool;
    fn record(&self, tool: &str, args_key: &str);
}

/// A process-scoped in-memory consent store. Entries expire after the
/// configured TTL; recall prunes expired entries lazily. The clock is the
/// monotonic Instant plus a test-controllable offset, so TTL expiry can be
/// simulated without sleeping and without the Instant-epoch fragility of
/// subtracting a duration backward.
pub struct InMemoryConsentStore {
    ttl: Duration,
    entries: Mutex<Vec<Entry>>,
    clock_offset: Mutex<Duration>,
}

#[derive(Debug, Clone)]
struct Entry {
    tool: String,
    args_key: String,
    recorded_at: Instant,
}

impl InMemoryConsentStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(Vec::new()),
            clock_offset: Mutex::new(Duration::ZERO),
        }
    }

    /// A store that never expires — useful for tests that exercise recall
    /// without racing the clock.
    pub fn non_expiring() -> Self {
        Self::new(Duration::from_secs(u64::MAX / 2))
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// The effective now: wall Instant plus the accumulated offset. The offset
    /// only moves forward (advance_clock adds to it), so entries age out.
    fn effective_now(&self) -> Instant {
        Instant::now() + *self.clock_offset.lock().expect("consent mutex")
    }

    /// Advance the logical clock forward by delta so existing entries age out
    /// without sleeping. Only used by tests; production code never calls this.
    pub fn advance_clock(&self, delta: Duration) {
        *self.clock_offset.lock().expect("consent mutex") += delta;
    }
}

impl ConsentStore for InMemoryConsentStore {
    fn recall(&self, tool: &str, args_key: &str) -> bool {
        let mut entries = self.entries.lock().expect("consent mutex");
        let now = self.effective_now();
        let mut hit = false;
        entries.retain(|e| {
            let fresh = now.duration_since(e.recorded_at) <= self.ttl;
            if !fresh {
                return false;
            }
            if e.tool == tool && e.args_key == args_key {
                hit = true;
            }
            true
        });
        hit
    }

    fn record(&self, tool: &str, args_key: &str) {
        let mut entries = self.entries.lock().expect("consent mutex");
        // Replace any existing entry for the same key (refresh the TTL).
        let now = self.effective_now();
        entries.retain(|e| !(e.tool == tool && e.args_key == args_key));
        entries.push(Entry {
            tool: tool.into(),
            args_key: args_key.into(),
            recorded_at: now,
        });
    }
}

/// Derive a stable args key from an optional JSON input. Two calls with the
/// same serialized input map to the same key so a stored consent for one edit
/// does not auto-allow a different edit. None input keys as empty.
pub fn args_key(input: Option<&serde_json::Value>) -> String {
    match input {
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests;
