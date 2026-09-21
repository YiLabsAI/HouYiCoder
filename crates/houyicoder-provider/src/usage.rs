//! Usage-field mapping for OpenAI-compatible responses.

use houyicoder_protocol::llm::Usage;
use serde_json::Value;

/// Map the OpenAI usage object to inclusive token totals. Cached input and
/// reasoning are subsets of prompt and completion tokens respectively.
pub(crate) fn parse_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    fn get(value: &Value, key: &str) -> u32 {
        value.get(key).and_then(Value::as_u64).unwrap_or(0) as u32
    }
    let input_tokens = get(usage, "prompt_tokens");
    let output_tokens = get(usage, "completion_tokens");
    let total_tokens = get(usage, "total_tokens");
    let completion_details = usage
        .get("completion_tokens_details")
        .unwrap_or(&Value::Null);
    let prompt_details = usage.get("prompt_tokens_details").unwrap_or(&Value::Null);
    let reasoning_tokens = get(completion_details, "reasoning_tokens");
    let cached_input = get(prompt_details, "cached_tokens");
    Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        non_cached_input_tokens: input_tokens.saturating_sub(cached_input),
        cache_read_input_tokens: cached_input,
        reasoning_tokens,
        ..Default::default()
    }
}
