//! What a session's delegated children spent, summed from the durable returns
//! they left in the parent's log.
//!
//! This is the one source for delegated usage. The child's own tool result
//! carries a usage block, but it is written when the tool returns: a background
//! launch returns before the child runs, and an interrupted delegation returns
//! without a block, so that path understates the cost. The durable
//! SubagentReturn is written when the child reaches a terminal on every path,
//! which makes it the only complete record.

use houyicoder_context::{SessionEvent, SessionLogEntry};
use houyicoder_protocol::llm::Usage;

/// The delegated usage a session's children reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubagentUsage {
    /// Children that reached a terminal, whether or not they reported usage.
    pub calls: usize,
    /// Children that reached a terminal without reporting any usage. Their cost
    /// is unknown, so a caller that sums totals must treat it as such rather
    /// than as a free child.
    pub unmeasured_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl SubagentUsage {
    /// The same figures as a provider usage record, for folding into a session
    /// tally. Saturates at the u32 ceiling the provider type uses.
    pub fn to_usage(self) -> Usage {
        let clamp = |v: u64| v.min(u32::MAX as u64) as u32;
        let input_tokens = clamp(self.input_tokens);
        let output_tokens = clamp(self.output_tokens);
        Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            non_cached_input_tokens: clamp(
                self.input_tokens
                    .saturating_sub(self.cache_read_input_tokens),
            ),
            cache_read_input_tokens: clamp(self.cache_read_input_tokens),
            cache_write_input_tokens: clamp(self.cache_write_input_tokens),
            reasoning_tokens: clamp(self.reasoning_tokens),
        }
    }
}

/// Sum every delegated child's return in a session log.
///
/// A return with no usage block counts as a call whose cost is unknown: the
/// caller can say a child ran, and must not claim it spent nothing.
pub fn aggregate_subagent_usage(events: &[SessionLogEntry]) -> SubagentUsage {
    let mut total = SubagentUsage::default();
    for ev in events {
        let SessionEvent::SubagentReturn {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            ..
        } = &ev.event
        else {
            continue;
        };
        total.calls += 1;
        if *input_tokens == 0
            && *output_tokens == 0
            && *cache_read_input_tokens == 0
            && *cache_write_input_tokens == 0
            && *reasoning_tokens == 0
        {
            total.unmeasured_calls += 1;
        }
        total.input_tokens = total.input_tokens.saturating_add(*input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(*output_tokens);
        total.cache_read_input_tokens = total
            .cache_read_input_tokens
            .saturating_add(*cache_read_input_tokens);
        total.cache_write_input_tokens = total
            .cache_write_input_tokens
            .saturating_add(*cache_write_input_tokens);
        total.reasoning_tokens = total.reasoning_tokens.saturating_add(*reasoning_tokens);
    }
    total
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
