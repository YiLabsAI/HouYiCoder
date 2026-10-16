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

use crate::cache_policy::CacheBreakpoint;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::time::Duration;

/// Which token-breakdown fields the source endpoint reported. A field the
/// endpoint did not emit is unreported (unknown), not zero; a reported field
/// at zero is a confirmed zero. Carried alongside the u32 values so a missing
/// cache-read count reads as unknown rather than as a 0% hit rate. Defaults to
/// all-unreported: a record that predates the field, or one a provider built
/// without setting provenance, is treated as unreported and left out of the
/// measured group rather than read as a 0% sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageProvenance {
    /// Whether the endpoint reported the inclusive input total.
    pub input_reported: bool,
    /// Whether the endpoint reported the cache-read count.
    pub cache_read_reported: bool,
    /// Whether the endpoint reported the cache-write count. Split-accounting
    /// providers carry a separate write count; subset-accounting providers do
    /// not, so this stays false there and is not read as a missing write.
    pub cache_write_reported: bool,
}

impl UsageProvenance {
    /// Every field reported. This is the identity for the merge AND-fold: a
    /// running total seeded from it stays reported until an unreported part is
    /// folded in, so the first recorded turn decides the total's completeness.
    /// A sum of zero measured turns is complete, not unknown.
    pub const ALL_REPORTED: UsageProvenance = UsageProvenance {
        input_reported: true,
        cache_read_reported: true,
        cache_write_reported: true,
    };

    /// Whether the cache account is complete enough to compute a trustworthy
    /// hit rate: input and cache-read both reported. Records below this bar
    /// yield an unknown rate rather than a misleading 0%.
    pub fn cache_account_complete(self) -> bool {
        self.input_reported && self.cache_read_reported
    }

    /// The share of a cache account that came from the prompt cache, as a rate
    /// in 0..1. None unless the account is complete — the endpoint reported both
    /// the input total and the cache-read count — and the input is above zero.
    ///
    /// The gate carries the whole meaning here. A count above zero is a floor on
    /// what was cached, but a quotient over a partial account divides that floor
    /// by an input sum that is short by whatever went unreported, so the share
    /// can read higher than the truth. An account that did not report both
    /// numbers yields None, which callers render as unknown rather than as a
    /// percentage. A provider whose cache-read count exceeds its input would
    /// push the rate above one, so it is held to the range this documents.
    pub fn cache_hit_rate(self, cache_read: u64, input: u64) -> Option<f64> {
        if !self.cache_account_complete() || input == 0 {
            return None;
        }
        Some((cache_read as f64 / input as f64).min(1.0))
    }

    /// The same share as a percentage, the unit the panes render. One formula
    /// behind every per-call and per-delegation share a pane derives, so no two
    /// of them can round or floor it apart.
    pub fn cache_hit_pct(self, cache_read: u64, input: u64) -> Option<f64> {
        self.cache_hit_rate(cache_read, input)
            .map(|rate| rate * 100.0)
    }

    /// The merge a combined usage record takes: a field is reported in the sum
    /// only when every summand reported it, so an unreported part cannot hide
    /// inside a total that reads as complete.
    pub fn merge(self, other: UsageProvenance) -> UsageProvenance {
        UsageProvenance {
            input_reported: self.input_reported && other.input_reported,
            cache_read_reported: self.cache_read_reported && other.cache_read_reported,
            cache_write_reported: self.cache_write_reported && other.cache_write_reported,
        }
    }
}

/// Whether a recorded count belongs on a row: the endpoint said it reported the
/// field, or the count itself is above zero.
///
/// A zero no call reported is unknown, not a measured miss, so it stays off the
/// row. A zero the endpoint did report is kept — a miss the provider measured is
/// worth seeing — and a count above zero is shown even when the account behind
/// it is incomplete, because a floor is honest where a quotient over that
/// account is not. Only counts go through this; a share is derived from the
/// complete-account rule instead.
pub fn count_visible(count: u64, reported: bool) -> bool {
    reported || count > 0
}

/// The token counts a durable record keeps for one turn, before they are
/// normalized. A record keeps the counts and whether the endpoint reported
/// each of them, but no total: the total is derived here, so every reader of
/// the log derives the same one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordedCounts {
    /// Inclusive input tokens (includes cache-read and cache-write).
    pub input_tokens: u64,
    /// Inclusive output tokens (includes reasoning).
    pub output_tokens: u64,
    /// Cache-read input tokens (reused prefix).
    pub cache_read_input_tokens: u64,
    /// Cache-write input tokens (new prefix written to the cache).
    pub cache_write_input_tokens: u64,
    /// Reasoning tokens (a subset of output_tokens).
    pub reasoning_tokens: u64,
    /// Which of the counts above the endpoint reported.
    pub provenance: UsageProvenance,
}

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
    /// Which breakdown fields the source endpoint reported; a field not
    /// reported is unknown, not zero. Defaults to all-unreported for records
    /// that predate the field, so old logs deserialize as unreported and are
    /// left out of the measured group rather than read as 0% samples.
    #[serde(default)]
    pub provenance: UsageProvenance,
}

impl Usage {
    /// The output tokens the user sees (excludes reasoning). The only
    /// subtracting accessor; saturates so it never underflows.
    pub fn visible_output_tokens(&self) -> u32 {
        self.output_tokens.saturating_sub(self.reasoning_tokens)
    }

    /// The share of this usage's input that came from the prompt cache, as a
    /// rate in 0..1. The provenance decides whether the account supports a
    /// share at all, so a record whose endpoint did not report both counts
    /// yields None and its reader shows unknown rather than a 0% that reads
    /// like a cache miss.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        self.provenance.cache_hit_rate(
            self.cache_read_input_tokens as u64,
            self.input_tokens as u64,
        )
    }

    /// The same share as a percentage, the unit the panes render. One formula
    /// behind every per-call and per-delegation share a pane derives, so no two
    /// of them can round or floor it apart.
    pub fn cache_hit_pct(&self) -> Option<f64> {
        self.provenance.cache_hit_pct(
            self.cache_read_input_tokens as u64,
            self.input_tokens as u64,
        )
    }

    /// Sum two usage records field by field, saturating at the u32 ceiling.
    /// Used to combine a session's own provider calls with the usage its
    /// delegated children reported.
    pub fn saturating_add(&self, other: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            non_cached_input_tokens: self
                .non_cached_input_tokens
                .saturating_add(other.non_cached_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_add(other.cache_write_input_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
            provenance: self.provenance.merge(other.provenance),
        }
    }

    /// Normalize the counts a durable record carries: the total is input plus
    /// output, the non-cached input is input minus both cache fields, and
    /// counts above the u32 ceiling saturate. The record holds no total of its
    /// own, so one is derived here; every path that rebuilds a turn's usage
    /// from the log normalizes it through this function, so a session's own
    /// tally and the per-model table cannot disagree about what a turn cost.
    pub fn from_recorded(counts: RecordedCounts) -> Self {
        let clamp = |v: u64| v.min(u32::MAX as u64) as u32;
        let input_tokens = clamp(counts.input_tokens);
        let output_tokens = clamp(counts.output_tokens);
        Self {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            non_cached_input_tokens: clamp(
                counts
                    .input_tokens
                    .saturating_sub(counts.cache_read_input_tokens)
                    .saturating_sub(counts.cache_write_input_tokens),
            ),
            cache_read_input_tokens: clamp(counts.cache_read_input_tokens),
            cache_write_input_tokens: clamp(counts.cache_write_input_tokens),
            reasoning_tokens: clamp(counts.reasoning_tokens),
            provenance: counts.provenance,
        }
    }

    /// The additive identity for folding usage records: zero token totals with
    /// all-fields-reported provenance. A saturating_add against it leaves the
    /// other record's totals and completeness unchanged, so an accumulator
    /// seeded from it still reads complete for a zero-turn session and takes
    /// its completeness from the first recorded turn. A zero-input identity has
    /// no cache_hit_rate of its own, but unlike the all-unreported default it
    /// does not dilute the first real record.
    pub fn identity() -> Self {
        Self {
            provenance: UsageProvenance::ALL_REPORTED,
            ..Self::default()
        }
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
/// translates its SessionLogEntry log into these (user/assistant text + tool
/// results). Tagged for wire transport. An Assistant item may carry the tool
/// calls it emitted alongside its text — the tool_use/tool_result pair
/// invariant must survive every mapping (a ToolResult without its matching
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl EffortLevel {
    /// The lowercase name shared by the serde form and the durable TurnUsage
    /// record, so the two spellings never drift.
    pub fn label(self) -> &'static str {
        match self {
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
            EffortLevel::XHigh => "xhigh",
            EffortLevel::Max => "max",
        }
    }
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
    /// The provider service tier to request. The canonical form is the speed
    /// mode; the provider's raw tier string lives only in the lowering layer.
    #[serde(default)]
    pub speed: Option<SpeedMode>,
}

/// The provider service tier a request asks for. Only the tiers the product
/// contract defines exist here; the wire string each provider expects is the
/// lowering layer's concern.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeedMode {
    #[default]
    Standard,
    Fast,
}

impl SpeedMode {
    /// The label the transcript and pane print.
    pub fn label(self) -> &'static str {
        match self {
            SpeedMode::Standard => "off",
            SpeedMode::Fast => "on",
        }
    }
}

/// A prepared model-call request. Instructions is a static string (the loop
/// resolved any dynamic Instructions::Dynamic closure already).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    pub instructions: String,
    /// Byte offset into instructions where the cwd-dependent dynamic suffix
    /// begins. The provider hashes only the segment before it into the
    /// prompt-cache key, so a working-directory or environment change confined
    /// to the suffix keeps the key stable instead of rotating it. The segment
    /// before the boundary is stable across a cwd change but not byte-identical
    /// across every turn: the memory index lives there and is rewritten at a
    /// refresh. None means no boundary was threaded, so the whole instructions
    /// counts as static and the key behaves as it did before the offset
    /// existed. The field is optional under serde so a request serialized
    /// without it reads back None.
    #[serde(default)]
    pub dynamic_boundary: Option<usize>,
    pub input: Vec<InputItem>,
    pub tools: Vec<ToolDef>,
    pub settings: ModelSettings,
    /// Prompt-cache breakpoints a cache policy placed on the request. The
    /// provider lowers each symbolic kind to a concrete position in its own
    /// wire format; an empty vec means no cache hints (the provider does not
    /// carve a prefix beyond its own defaults).
    #[serde(default)]
    pub cache_breakpoints: Vec<CacheBreakpoint>,
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
/// the run_again rule) and appends a SessionLogEntry to the session.
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
    pub fn retry_after_delay(&self) -> Option<Duration> {
        match self {
            Self::RateLimit {
                retry_after_ms: Some(ms),
            } => Some(Duration::from_millis(*ms)),
            _ => None,
        }
    }
}

impl Display for ProviderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
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

impl Error for ProviderError {}

#[cfg(test)]
mod tests;
