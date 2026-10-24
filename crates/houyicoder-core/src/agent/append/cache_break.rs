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

/// Ephemeral prompt-cache time-to-live (5 minutes in milliseconds). When the
/// prefix is unchanged and the gap since the previous response is at least
/// this long, a cache miss is explained by provider TTL expiry.
const EPHEMERAL_CACHE_TTL_MS: u64 = 5 * 60 * 1000;

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
        if let Some((previous, previous_at)) = previous
            && let Some(current) = self.context_builder.prompt_hash()
        {
            if current.system != previous.system {
                return Some("system-prompt-changed");
            }
            if current.tools != previous.tools {
                return Some("tools-changed");
            }
            // The prefix is identical, so the miss is the provider letting
            // its cache expire while the session sat idle.
            if now_millis().saturating_sub(previous_at) >= EPHEMERAL_CACHE_TTL_MS {
                return Some("ttl-expired");
            }
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
