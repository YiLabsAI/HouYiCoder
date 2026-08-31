//! Silent-truncation heuristics: detect when a streamed reply was cut at the
//! output-token cap (a "length" finish) or mid code-block (an odd fence
//! count). Pure functions, no Runner state; extracted from call.rs so the
//! drive loop file stays under the size gate.

use houyicoder_context::TruncationSignal;
use houyicoder_protocol::llm::Usage;

/// Whether a provider finish reason means the reply was cut at the output
/// token cap. Providers spell it differently: OpenAI-compatible endpoints
/// say length, Anthropic-shaped replies say max_tokens, Gemini says
/// MAX_TOKENS (with variants like max_output_tokens seen from gateways).
/// Matched case-insensitively so the recovery loop keys on the meaning, not
/// one provider's spelling.
pub(super) fn is_length_reason(reason: &str) -> bool {
    matches!(
        reason.to_ascii_lowercase().as_str(),
        "length" | "max_tokens" | "max_output_tokens" | "model_length"
    )
}

/// Which silent-truncation signal fired for the folded turn state, or None
/// when no signal fired. Computed once so the drive loop both synthesizes
/// the cap-cut finish_reason and records the cause in the verdict without
/// re-running the heuristic. Priority: server-reported near-cap (most
/// reliable) > self-counted near-cap (fallback when the proxy omits usage) >
/// unclosed code block. The server count is checked only when the server
/// reported one — matches the original heuristic's prefer-server behavior.
pub(super) fn classify_truncation_signal(
    assistant_text: &str,
    usage: &Usage,
    self_count: u32,
    max_output_tokens: u32,
) -> TruncationSignal {
    const SILENT_TRUNCATION_SLACK: u32 = 64;
    let cap = max_output_tokens.saturating_sub(SILENT_TRUNCATION_SLACK);
    if usage.output_tokens != 0 {
        if usage.output_tokens >= cap {
            return TruncationSignal::ServerUsageNearCap;
        }
    } else if self_count >= cap {
        return TruncationSignal::SelfCountNearCap;
    }
    if !assistant_text.is_empty() && count_triple_backticks(assistant_text) % 2 == 1 {
        return TruncationSignal::UnclosedCodeBlock;
    }
    TruncationSignal::None
}

/// Count triple-backtick occurrences in a string. An odd count means an open
/// code fence never closed — a signal the stream was cut mid-block. Used by
/// the silent-truncation heuristic to catch a proxy that cut the reply but
/// signaled a natural stop.
fn count_triple_backticks(s: &str) -> usize {
    let mut count = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 2 < bytes.len() {
        if bytes[i] == b'`' && bytes[i + 1] == b'`' && bytes[i + 2] == b'`' {
            count += 1;
            i += 3;
        } else {
            i += 1;
        }
    }
    count
}
