//! Reading a delegation's own usage out of its tool result.

use houyicoder_protocol::llm::Usage;

/// Read a delegation result's usage block, when it has one. A result without
/// the block (an older child, a launch that returned no outcome) reports
/// nothing rather than zeroes.
pub(super) fn subagent_usage(output: &serde_json::Value) -> Option<Usage> {
    let usage = output.get("usage")?;
    let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let input_tokens = field("input_tokens");
    let output_tokens = field("output_tokens");
    Some(Usage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens + output_tokens,
        non_cached_input_tokens: input_tokens.saturating_sub(field("cache_read_input_tokens")),
        cache_read_input_tokens: field("cache_read_input_tokens"),
        cache_write_input_tokens: field("cache_write_input_tokens"),
        reasoning_tokens: field("reasoning_tokens"),
    })
}
