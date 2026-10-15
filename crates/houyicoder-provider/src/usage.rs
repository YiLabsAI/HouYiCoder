//! Usage-field mapping for OpenAI-compatible responses.

use houyicoder_protocol::llm::{Usage, UsageProvenance};
use serde_json::Value;

/// Map the OpenAI usage object to inclusive token totals. Cached input and
/// reasoning are subsets of prompt and completion tokens respectively. The
/// cache-read count is read from the OpenAI-standard cached_tokens spelling
/// or, when a gateway emits only DeepSeek's native prompt_cache_hit_tokens,
/// from that field. A field the response did not carry is marked unreported
/// in the provenance, so a missing cache-read count reads as unknown rather
/// than as 0%; a reported zero stays a confirmed zero. The OpenAI-compatible
/// object may carry a cache-write count under the cache_creation_input_tokens
/// key when the endpoint relays Anthropic-shaped usage through a compat
/// gateway; absent the key, cache-write stays unreported.
pub(crate) fn parse_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    fn reported_u64(value: &Value, key: &str) -> Option<u64> {
        value.get(key).and_then(Value::as_u64)
    }
    let input_opt = reported_u64(usage, "prompt_tokens");
    let output_opt = reported_u64(usage, "completion_tokens");
    let total_opt = reported_u64(usage, "total_tokens");
    let completion_details = usage
        .get("completion_tokens_details")
        .unwrap_or(&Value::Null);
    let prompt_details = usage.get("prompt_tokens_details").unwrap_or(&Value::Null);
    let reasoning_opt = reported_u64(completion_details, "reasoning_tokens");
    let cached_opt = reported_u64(prompt_details, "cached_tokens")
        .or_else(|| reported_u64(usage, "prompt_cache_hit_tokens"));
    let cache_write_opt = reported_u64(usage, "cache_creation_input_tokens");
    let input_tokens = input_opt.unwrap_or(0) as u32;
    let output_tokens = output_opt.unwrap_or(0) as u32;
    let total_tokens = total_opt.unwrap_or(0) as u32;
    let reasoning_tokens = reasoning_opt.unwrap_or(0) as u32;
    let cached_input = cached_opt.unwrap_or(0) as u32;
    let cache_write = cache_write_opt.unwrap_or(0) as u32;
    Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        non_cached_input_tokens: input_tokens
            .saturating_sub(cached_input)
            .saturating_sub(cache_write),
        cache_read_input_tokens: cached_input,
        cache_write_input_tokens: cache_write,
        reasoning_tokens,
        provenance: UsageProvenance {
            input_reported: input_opt.is_some(),
            cache_read_reported: cached_opt.is_some(),
            cache_write_reported: cache_write_opt.is_some(),
        },
    }
}
