//! Why the cached prefix broke.
//!
//! The provider reports a cache miss through a drop in cache-read tokens; that
//! number says the prefix changed but not what changed. This module names the
//! cause, from the most specific to the least, so a reader chasing a cache
//! regression knows which half of the prefix to look at.

use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use super::Runner;
use crate::agent::context::cache_liveness::CACHE_TTL_MS;

impl Runner {
    /// Name what broke the cached prefix.
    ///
    /// Ordered from the most specific cause to the least: a boundary the user
    /// chose, a model switch, then a change in one half of the prefix, then
    /// the idle gap that let the provider's cache expire. Only when none of
    /// those applies is the cause genuinely unknown, and saying so is honest
    /// rather than a guess.
    pub(super) fn cache_break_cause(&self) -> Option<&'static str> {
        if self.cache_compact_flag.load(Ordering::Relaxed) {
            return Some("compact");
        }
        if self.cache_model_switch_flag.load(Ordering::Relaxed) {
            return Some("model-switch");
        }
        let previous = self.cache_prev_prefix.lock().ok().and_then(|slot| *slot);
        if let Some((previous, previous_at)) = previous {
            if let Some(current) = self.context_builder.prompt_hash() {
                if current.system != previous.system {
                    return Some("system-prompt-changed");
                }
                if current.tools != previous.tools {
                    return Some("tools-changed");
                }
            }
            // The system and tools halves are unchanged; a retention demotion
            // in this turn's assembly is the specific cause of the drop.
            if self.cached_prefix.retention_demoted() {
                return Some("retention-demoted");
            }
            // The prefix is identical, so the miss is the provider letting
            // its cache expire while the session sat idle.
            if now_millis().saturating_sub(previous_at) >= CACHE_TTL_MS {
                return Some("ttl-expired");
            }
            return Some("unknown");
        }
        if self.cached_prefix.retention_demoted() {
            return Some("retention-demoted");
        }
        // A reopened session has no prefix hash from this process yet, but it
        // restored the last completion's clock. An idle gap at or past the
        // declared TTL is the one cause that evidence can still name.
        let seeded = self.cached_prefix.last_completion_ts();
        if seeded > 0 && now_millis().saturating_sub(seeded) >= CACHE_TTL_MS {
            return Some("ttl-expired");
        }
        Some("unknown")
    }
}

/// The wall clock in milliseconds, for the idle-gap comparison.
pub(super) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
