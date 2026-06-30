//! LLM streaming vocabulary: the LlmEvent taxonomy and Usage struct.
//!
//! The normalized streaming events a provider emits and a frontend renders.
//! One event stream, no dual v1/v2 write (a dual-version migration debt is
//! what we avoid).
//!
//! Usage carries inclusive totals (input_tokens includes cached, output_tokens
//! includes reasoning, total_tokens) plus a non-overlapping breakdown
//! (non_cached + cache_read + cache_write = input_tokens; reasoning <=
//! output_tokens). Every field is stored independently so consumers never
//! subtract to recover a category — eliminates the underflow class where a
//! clamped difference silently stores the wrong value. The single subtraction
//! is visible_output_tokens (output - reasoning, saturating).

use serde::{Deserialize, Serialize};

/// Token usage for one model call. Inclusive totals plus a non-overlapping
/// breakdown; see the module doc for the invariants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Inclusive input tokens (includes cache-read and cache-write). Aliased
    /// to the OpenAI-compatible field name prompt_tokens so a DashScope/OpenAI
    /// usage JSON deserializes correctly (the Anthropic name is input_tokens).
    #[serde(alias = "prompt_tokens")]
    pub input_tokens: u32,
    /// Inclusive output tokens (includes reasoning). Aliased to the
    /// OpenAI-compatible completion_tokens.
    #[serde(alias = "completion_tokens")]
    pub output_tokens: u32,
    /// Total tokens (input + output, as the provider reports it).
    pub total_tokens: u32,
    /// Non-cached input tokens (the bytes actually processed this call).
    pub non_cached_input_tokens: u32,
    /// Cache-read input tokens (reused prefix, free or discounted).
    pub cache_read_input_tokens: u32,
    /// Cache-write input tokens (new prefix written to the cache).
    pub cache_write_input_tokens: u32,
    /// Reasoning tokens (a subset of output_tokens).
    pub reasoning_tokens: u32,
}

impl Usage {
    /// The output tokens the user sees (excludes reasoning). The only
    /// subtracting accessor; saturates so it never underflows.
    pub fn visible_output_tokens(&self) -> u32 {
        self.output_tokens.saturating_sub(self.reasoning_tokens)
    }
}

/// One normalized streaming event from a model provider. Tagged for wire
/// transport. A provider impl adapts its raw stream into these; the agent
/// loop and frontends consume only this taxonomy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LlmEvent {
    StepStart {
        index: u32,
    },
    StepFinish {
        index: u32,
        reason: String,
        usage: Option<Usage>,
    },
    TextStart {
        id: String,
    },
    TextDelta {
        id: String,
        text: String,
    },
    TextEnd {
        id: String,
    },
    ReasoningStart {
        id: String,
    },
    ReasoningDelta {
        id: String,
        text: String,
    },
    ReasoningEnd {
        id: String,
    },
    ToolInputStart {
        id: String,
        name: String,
    },
    ToolInputDelta {
        id: String,
        name: String,
        text: String,
    },
    ToolInputEnd {
        id: String,
        name: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        name: String,
        output: serde_json::Value,
    },
    ToolError {
        id: String,
        name: String,
        message: String,
    },
    Finish {
        reason: String,
        usage: Option<Usage>,
    },
    ProviderError {
        message: String,
        retryable: Option<bool>,
    },
}

/// A tool call the assistant made in a prior turn, replayed into history so
/// the model can match its tool_result by id. Matches OutputItem::ToolCall.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// One history item sent to the model in a CompletionRequest. The loop
/// translates its TurnEvent log into these (user/assistant text + tool
/// results). Tagged for wire transport. An Assistant item may carry the tool
/// calls it emitted alongside its text — the tool_use/tool_result pair
/// invariant must survive every projection (a ToolResult without its matching
/// AssistantToolCall in the window is a bug).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum InputItem {
    User {
        content: String,
    },
    Assistant {
        content: String,
        /// Tool calls this assistant turn produced, in emit order. Empty when
        /// the assistant emitted only text. Kept here (not a separate item) so
        /// one Assistant item + its ToolResults reconstruct one API message.
        tool_calls: Vec<AssistantToolCall>,
    },
    /// A tool result answering a tool call by id (the tool_use/tool_result
    /// pair invariant — never emit one without the other in the same window).
    ToolResult {
        call_id: String,
        output: serde_json::Value,
    },
}

/// A tool declaration the model can choose to call. The input_schema is a JSON
/// Schema (from schemars or hand-written); the description is capped at the
/// provider's limit by the loop before sending (the CLI's 2048-char discipline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Per-call model settings. The loop caps max_output_tokens (the CLI's
/// CAPPED_DEFAULT_MAX_TOKENS=8000 + one 64k retry discipline) before filling
/// these.
/// User-selectable reasoning effort for models that accept it. The auto
/// state (no explicit choice) is carried by Option<EffortLevel>, not a
/// variant — the wire never sends an "auto" string, a default-resolution
/// layer decides what concrete level (if any) applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelSettings {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    /// reasoning_effort for the o1/o3/gpt-5 family.
    #[serde(default)]
    pub reasoning_effort: Option<EffortLevel>,
    /// enable_thinking flag for the qwen3 family.
    #[serde(default)]
    pub enable_thinking: Option<bool>,
    /// thinking_budget for the qwen3 family (clamped below max_output_tokens).
    #[serde(default)]
    pub thinking_budget: Option<u32>,
}

/// A prepared model-call request. Instructions is a static string (the loop
/// resolved any dynamic Instructions::Dynamic closure already).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputItem>,
    pub tools: Vec<ToolDef>,
    pub settings: ModelSettings,
    /// Prompt-cache breakpoints a cache policy placed on the request. The
    /// provider lowers each symbolic kind to a concrete position in its own
    /// wire format; an empty vec means no cache hints (the provider does not
    /// carve a prefix beyond its own defaults).
    #[serde(default)]
    pub cache_breakpoints: Vec<crate::cache_policy::CacheBreakpoint>,
}

/// One output item from a model response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Reasoning {
        text: String,
    },
}

/// A complete model response: the output items plus usage. The loop inspects
/// the output for tool calls (has_tools_or_approvals_to_run → run_again,
/// the run_again rule) and appends a TurnEvent to the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub output: Vec<OutputItem>,
    pub usage: Usage,
    pub model: String,
}

impl CompletionResponse {
    /// True when the response carries any tool call — the run_again rule's
    /// basis (has_tools_or_approvals_to_run). A turn with pending
    /// tools is never final.
    pub fn has_tool_calls(&self) -> bool {
        self.output
            .iter()
            .any(|o| matches!(o, OutputItem::ToolCall { .. }))
    }
}

/// What a model can do (capability negotiation, matches MemoryProvider). The
/// loop gates optional features (streaming, tools, vision) on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub vision: bool,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            streaming: false,
            tools: true,
            vision: false,
            context_window: 200_000,
            max_output_tokens: 8_000,
        }
    }
}

/// Errors a provider can return. A 10-way reason taxonomy: RateLimit (retryable, 429 with retry-after) is split from
/// ProviderInternal (retryable, 5xx); QuotaExceeded (429 but billing/quota —
/// NOT retryable) is split from RateLimit; ContextOverflow (the compaction
/// trigger signal) is split from InvalidRequest. Every error stores its own
/// retryable verdict so the loop's Retry never subtracts to recover a class.
#[derive(Debug, Clone)]
pub enum ProviderError {
    /// Connection, timeout, or transport-level failure. Retryable.
    Network,
    /// 429 with an optional Retry-After (ms). Retryable.
    RateLimit { retry_after_ms: Option<u64> },
    /// 429 caused by quota/billing exhaustion (not a transient rate limit).
    /// Distinct from RateLimit so the loop does NOT retry burning budget.
    QuotaExceeded,
    /// 401/403 — authentication or permission. Not retryable.
    Auth,
    /// 404 or a 400/422 whose body names the model — the model id does not
    /// exist on the provider. Not retryable. Split from InvalidRequest so the
    /// caller's error message points at the model id / catalog, not at auth.
    ModelNotFound(String),
    /// 400/404/409/422 — the request itself is wrong. Not retryable.
    InvalidRequest(String),
    /// Context window exceeded (413 or a context-overflow body). Not
    /// retryable; the agent loop uses this to trigger compaction. When the
    /// provider's error body named the real enforced limit, it is carried
    /// here so the catalog can be corrected at runtime — the provider's
    /// enforced value is ground truth, authoritative over the static
    /// catalog and the [1m] opt-in (a provider that will not serve 1M
    /// cannot be opted into it).
    ContextOverflow { enforced_limit: Option<u32> },
    /// Content-filter/safety block. Not retryable.
    ContentPolicy,
    /// 5xx — provider-internal, transient. Retryable.
    ProviderInternal,
    /// Anything not classified above.
    Unknown(String),
}

impl ProviderError {
    /// True when a retry is worth attempting (the loop's Retry::run_if gates
    /// the max attempts; this is the per-error classification). The union
    /// of retryable reasons (RateLimit + ProviderInternal +
    /// Transport/Network).
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Network | Self::RateLimit { .. } | Self::ProviderInternal
        )
    }

    /// A server-suggested retry delay, when the provider returned one (the
    /// Retry-After header on a 429). The retry loop honors this as a server
    /// directive that bypasses the computed backoff ceiling — a rate-limited
    /// account is polled after the server's window, not every max-delay tick.
    /// None when the error carries no directive (backoff applies).
    pub fn retry_after_delay(&self) -> Option<std::time::Duration> {
        match self {
            Self::RateLimit {
                retry_after_ms: Some(ms),
            } => Some(std::time::Duration::from_millis(*ms)),
            _ => None,
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network => write!(f, "provider network error"),
            Self::RateLimit { retry_after_ms } => {
                write!(
                    f,
                    "provider rate-limited (retry_after_ms: {retry_after_ms:?})"
                )
            }
            Self::QuotaExceeded => write!(f, "provider quota exceeded"),
            Self::Auth => write!(f, "provider auth error"),
            Self::ModelNotFound(m) => write!(f, "model not found: {m}"),
            Self::InvalidRequest(m) => write!(f, "invalid request: {m}"),
            Self::ContextOverflow { .. } => write!(f, "context window exceeded"),
            Self::ContentPolicy => write!(f, "content policy violation"),
            Self::ProviderInternal => write!(f, "provider internal error"),
            Self::Unknown(m) => write!(f, "unknown provider error: {m}"),
        }
    }
}

impl std::error::Error for ProviderError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effort_serde_lowercase() {
        assert_eq!(serde_json::to_string(&EffortLevel::Low).unwrap(), "\"low\"");
        assert_eq!(
            serde_json::to_string(&EffortLevel::Medium).unwrap(),
            "\"medium\""
        );
        assert_eq!(
            serde_json::to_string(&EffortLevel::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::from_str::<EffortLevel>("\"high\"").unwrap(),
            EffortLevel::High
        );
    }

    #[test]
    fn test_effort_three_state_roundtrips() {
        // Each level survives a serialize -> deserialize cycle unchanged,
        // and an absent effort (Option::None) round-trips as null — the
        // durable-log + wire callers rely on the lowercase wire form.
        for level in [EffortLevel::Low, EffortLevel::Medium, EffortLevel::High] {
            let json = serde_json::to_string(&level).unwrap();
            let back: EffortLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level, "round-trip {level:?} via {json}");
        }
        let absent: Option<EffortLevel> = None;
        let json = serde_json::to_string(&absent).unwrap();
        assert_eq!(json, "null");
        let back: Option<EffortLevel> = serde_json::from_str(&json).unwrap();
        assert!(back.is_none(), "absent effort round-trips as None");
    }

    #[test]
    fn test_usage_visible_output_saturates() {
        let u = Usage {
            output_tokens: 5,
            reasoning_tokens: 8,
            ..Default::default()
        };
        assert_eq!(u.visible_output_tokens(), 0); // saturates, no underflow.
    }

    #[test]
    fn test_llm_event_serde_round() {
        let e = LlmEvent::TextDelta {
            id: "b1".into(),
            text: "hi".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: LlmEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
        assert!(json.contains("\"type\":\"TextDelta\""));
    }

    #[test]
    fn test_rate_limit_carries_delay() {
        // Only RateLimit with a Retry-After carries a server delay; bare
        // RateLimit + other retryable errors carry none (backoff applies).
        assert_eq!(
            ProviderError::RateLimit {
                retry_after_ms: Some(5_000)
            }
            .retry_after_delay(),
            Some(std::time::Duration::from_millis(5_000))
        );
        assert_eq!(
            ProviderError::RateLimit {
                retry_after_ms: None
            }
            .retry_after_delay(),
            None
        );
        assert_eq!(ProviderError::Network.retry_after_delay(), None);
        assert_eq!(ProviderError::ProviderInternal.retry_after_delay(), None);
    }
}
