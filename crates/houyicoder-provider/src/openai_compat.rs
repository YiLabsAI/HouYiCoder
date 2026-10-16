//! OpenAI-compatible provider: a real ModelProvider backed by the
//! chat/completions HTTP protocol. This is the "not bound to any one vendor"
//! seam — point it at any OpenAI-compatible endpoint (OpenAI itself, SiliconFlow,
//! DeepSeek, OpenRouter, a local vLLM/ollama, ...) by changing base_url +
//! api_key. The model id comes from the per-request CompletionRequest.
//!
//! Design deep-dives informed the shape:
//! - chat/completions endpoint, hook template, retry-on-retryable, replay
//!   (test replay) decoupled to a service layer.
//! - unified completion over the chat/completions protocol; retryable set
//!   {408,429,5xx}; Retry-After (integer seconds or a date string);
//!   exponential backoff + jitter; safety_checker (repeat-chunk detection
//!   — stream follow-up).
//! - one protocol, many providers via base_url/auth patch — zero adapter
//!   code per compatible vendor.
//! - emitted-visible-event veto (moot for non-streaming; lands with
//!   streaming).
//!
//! non-streaming (stream: false). Streaming (SSE + the ToolStream
//! by-index JSON-fragment accumulator + Lifecycle idempotent start/delta/end +
//! eager finishAll + the emitted/repeat-chunk vetoes) is a follow-up that wraps
//! the same protocol. Stateless (full history each call) ⇒ replay is always
//! server-safe, so Retry only honors ProviderError::retryable.

use std::time::Duration;

use crate::served_models::write_cache;
use crate::{
    http_error::{classify_with_body, map_reqwest_err, parse_retry_after},
    stream_decoder::{ChunkAction, StreamDecoder},
    usage::parse_usage,
};
use futures::StreamExt;
use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_protocol::cache_policy::{BreakpointKind, PromptCacheFormat};
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, EffortLevel, InputItem, LlmEvent, ModelCapabilities,
    OutputItem, ProviderError, SpeedMode,
};
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// An OpenAI-compatible chat/completions provider. Configure with base_url
/// (e.g. https://api.openai.com/v1) and api_key; the model is per-request.
/// The composition root resolves the key + base_url via the config layer and
/// hands them to new(); this type does no config resolution itself.
pub struct OpenAiCompatibleProvider {
    base_url: String,
    api_key: String,
    http: Client,
}

impl OpenAiCompatibleProvider {
    /// Construct a provider over an OpenAI-compatible endpoint.
    pub fn new(base_url: String, api_key: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(180))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client build");
        Self {
            base_url,
            api_key,
            http,
        }
    }

    /// Fetch GET {base_url}/models once and write the served-models cache at
    /// the given path. Path-explicit so a test can point at a temp file
    /// without mutating env (the workspace denies unsafe, so set_var is
    /// out). Fire-and-forget at startup; all failures degrade — network
    /// error, non-2xx, empty body, parse failure => return Err and keep the
    /// existing cache; the caller debug-logs, nothing surfaces. Skip-write
    /// when the parsed list equals the cached list.
    pub fn refresh_served_models_to(
        &self,
        cache_path: PathBuf,
    ) -> PFut<'_, Result<(), ProviderError>> {
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let http = self.http.clone();
        Box::pin(async move {
            let url = format!("{}/models", base_url.trim_end_matches('/'));
            let resp = http
                .get(&url)
                .bearer_auth(&api_key)
                .send()
                .await
                .map_err(map_reqwest_err)?;
            let status = resp.status();
            if !status.is_success() {
                let retry_after = parse_retry_after(resp.headers());
                let body = resp.text().await.unwrap_or_default();
                return Err(classify_with_body(status.as_u16(), retry_after, &body));
            }
            let json: Value = resp
                .json()
                .await
                .map_err(|_| ProviderError::Unknown("models response was not valid JSON".into()))?;
            let ids = crate::served_models::parse_response(&json);
            if ids.is_empty() {
                return Ok(());
            }
            let existing = houyicoder_config::load_ids_at(&cache_path);
            if existing == ids {
                return Ok(());
            }
            write_cache(&cache_path, &ids);
            Ok(())
        })
    }
}

impl ModelProvider for OpenAiCompatibleProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let http = self.http.clone();
        Box::pin(async move {
            let body = build_request_body(&req);
            let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
            let resp = http
                .post(&url)
                .bearer_auth(&api_key)
                .json(&body)
                .send()
                .await
                .map_err(map_reqwest_err)?;
            let status = resp.status();
            if !status.is_success() {
                let retry_after = parse_retry_after(resp.headers());
                let body = resp.text().await.unwrap_or_default();
                return Err(classify_with_body(status.as_u16(), retry_after, &body));
            }
            let json: Value = resp
                .json()
                .await
                .map_err(|_| ProviderError::Unknown("response body was not valid JSON".into()))?;
            parse_response(&json, &req.model)
        })
    }

    fn stream(
        &self,
        req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let http = self.http.clone();
        let stream = async_stream::stream! {
            let mut body = build_request_body(&req);
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage": true});
            let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
            let resp = match http.post(&url).bearer_auth(&api_key).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(map_reqwest_err(e));
                    return;
                }
            };
            if !resp.status().is_success() {
                let code = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                yield Err(classify_with_body(code, None, &text));
                return;
            }
            yield Ok(LlmEvent::StepStart { index: 0 });
            let mut buf = String::new();
            let mut byte_stream = resp.bytes_stream();
            let mut decoder = StreamDecoder::default();
            let mut out_events = Vec::new();
            'stream: while let Some(chunk) = byte_stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(map_reqwest_err(e));
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim().to_string();
                    buf = buf[pos + 1..].to_string();
                    if line.is_empty() || !line.starts_with("data: ") {
                        continue;
                    }
                    let data = &line[6..];
                    if data == "[DONE]" {
                        break 'stream;
                    }
                    let Ok(json) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };
                    out_events.clear();
                    let action = decoder.handle_chunk(&json, &mut out_events);
                    for ev in out_events.drain(..) {
                        yield Ok(ev);
                    }
                    if action == ChunkAction::BreakStream {
                        break 'stream;
                    }
                }
            }
            for ev in decoder.finish_events() {
                yield Ok(ev);
            }
        };
        Box::pin(stream)
    }

    /// Fetch GET {base_url}/models once and write the served-models cache.
    /// Fire-and-forget at startup: the host spawns this on the runtime and
    /// never awaits it. All failures degrade — network error, non-2xx, empty
    /// body, parse failure => return Err and keep the existing cache; the
    /// caller debug-logs, nothing surfaces to the user. Delegates to the
    /// path-explicit inherent method; the default path is the config-home
    /// cache.
    fn refresh_served_models(&self) -> PFut<'_, Result<(), ProviderError>> {
        self.refresh_served_models_to(houyicoder_config::cache_path())
    }

    fn capabilities(&self) -> ModelCapabilities {
        // The provider does not know the per-model context window —
        // OpenAI-compatible /v1/models returns only ids, not context-length.
        // Report 0 (unknown) so resolve_capabilities falls through to the
        // catalog (family table) + [1m] suffix + learned limits, which are
        // model-specific. A gateway that DOES report a real window would
        // override here and be trusted (non-zero wins over catalog).
        ModelCapabilities {
            context_window: 0,
            ..ModelCapabilities::default()
        }
    }
}

/// Accumulator for one streamed tool call. OpenAI streams a tool call across
/// many chunks keyed by index: the first chunk carries id + function.name, the
/// following chunks concatenate function.arguments fragments. We reassemble at
/// finish so the loop can dispatch the call (the input is not shown live as it
/// streams; only the reassembled call is).
#[derive(Default)]
pub(crate) struct ToolCallAccum {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args: String,
}

/// Decide the finish reason for a stream that ended without an explicit
/// finish_reason from the provider — an abnormal close. When text was in
/// flight, treat it as a mid-text cut (length) so the engine's length
/// recovery re-calls for the tail instead of silently accepting a truncated
/// reply; otherwise the close was clean (stop). A well-behaved gateway always
/// sends finish_reason on the final chunk, so reaching here with text in
/// flight means the close was not normal.
fn abnormal_close_reason(text_started: bool) -> &'static str {
    if text_started { "length" } else { "stop" }
}

/// Build the closing events for an abnormal stream end (the byte stream ended
/// without an explicit finish_reason from the provider). Closes any open
/// reasoning or text block, finalizes accumulated tool calls, and emits
/// StepFinish plus Finish whose reason is length when text was in flight
/// (a mid-text cut) or stop when no text was generated (a clean tool-call-only
/// or empty close). Pure so the close-gracefully path is unit-testable
/// without a mock SSE server (the stream body is HTTP-bound).
pub(crate) fn abnormal_close_events(
    reasoning_started: bool,
    text_started: bool,
    tool_calls: Vec<ToolCallAccum>,
    final_usage: Option<Usage>,
) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    if reasoning_started {
        events.push(LlmEvent::ReasoningEnd {
            id: "reason-0".into(),
        });
    }
    if text_started {
        events.push(LlmEvent::TextEnd {
            id: "text-0".into(),
        });
    }
    events.extend(finalize_tool_calls(tool_calls));
    let reason = abnormal_close_reason(text_started);
    events.push(LlmEvent::StepFinish {
        index: 0,
        reason: reason.into(),
        usage: final_usage.clone(),
    });
    events.push(LlmEvent::Finish {
        reason: reason.into(),
        usage: final_usage,
    });
    events
}

/// Merge one streamed tool-call fragment (a delta.tool_calls[i] JSON object)
/// into the by-index accumulator. The first chunk carries id + function.name;
/// arguments arrive as concatenated string fragments. Pure (no I/O) so the
/// reassembly is unit-testable without a mock SSE server.
pub(crate) fn accumulate_tool_call(acc: &mut Vec<ToolCallAccum>, tc: &Value) {
    let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
    if idx >= acc.len() {
        acc.resize_with(idx + 1, ToolCallAccum::default);
    }
    let slot = &mut acc[idx];
    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
        slot.id = id.into();
    }
    if let Some(f) = tc.get("function") {
        if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
            slot.name = name.into();
        }
        if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
            slot.args.push_str(args);
        }
    }
}

/// Make tool-call ids non-empty and unique within one response. Approval,
/// result, and model-history routing all key on call_id, so a duplicate can
/// route a decision or result to the wrong call. Minted ids use a process-wide
/// counter and reserve the houyi_tc_ prefix.
fn unique_id_gen() -> impl FnMut(&str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut seen: HashSet<String> = HashSet::new();
    move |raw: &str| {
        if raw.is_empty() || seen.contains(raw) {
            let id = format!("houyi_tc_{}", COUNTER.fetch_add(1, Ordering::Relaxed));
            seen.insert(id.clone());
            id
        } else {
            seen.insert(raw.to_string());
            raw.to_string()
        }
    }
}

/// Reassemble accumulated tool-call fragments into LlmEvent::ToolCall events
/// for the loop to dispatch. Entries with no name are dropped (a fragment that
/// never received its first chunk). Arguments that fail to parse as JSON fall
/// back to {} — the loop never panics on malformed tool input. Id uniqueness
/// is delegated to unique_id_gen (see its doc for the invariant and the
/// consumers that depend on it).
pub(crate) fn finalize_tool_calls(acc: Vec<ToolCallAccum>) -> Vec<LlmEvent> {
    let mut unique_id = unique_id_gen();
    acc.into_iter()
        .filter(|tc| !tc.name.is_empty())
        .map(|tc| {
            let input = serde_json::from_str(&tc.args).unwrap_or_else(|_| serde_json::json!({}));
            let id = unique_id(&tc.id);
            LlmEvent::ToolCall {
                id,
                name: tc.name,
                input,
            }
        })
        .collect()
}

/// Which effort dialect a model speaks, picked by a substring probe on the
/// model id. This is a dialect probe, not a validity check: a typo like
/// qwen3.8-max still matches qwen3, and a non-matching id like gpt-4o still
/// runs without effort. NotSupported only drives the effort row's
/// not-supported copy; it never adds a warning badge to the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortDialect {
    /// enable_thinking + thinking_budget.
    Qwen3,
    /// reasoning_effort.
    OpenaiReasoning,
    /// reasoning_effort.
    Glm,
    /// No effort parameter.
    NotSupported,
}

/// Probe the model id for its effort dialect. qwen3 wins over other families.
/// Keep in lockstep with the agent loop's copy (core cannot depend on this
/// crate) so the resolved dialect and the emitted fields cannot drift.
pub fn effort_dialect(model: &str) -> EffortDialect {
    let m = model.to_lowercase();
    if m.contains("qwen3") {
        EffortDialect::Qwen3
    } else if m.contains("o1")
        || m.contains("o3")
        || m.contains("gpt-5")
        || m.contains("deepseek-v4")
    {
        EffortDialect::OpenaiReasoning
    } else if m.contains("glm-5.2") || m.contains("glm-5.3") {
        EffortDialect::Glm
    } else {
        EffortDialect::NotSupported
    }
}

/// The wire string for an effort level (lowercase, matching the serde form).
fn effort_str(effort: EffortLevel) -> &'static str {
    effort.label()
}

/// Clamp a thinking budget below the output-token cap (invariant: budget <
/// max_output_tokens, so a request never asks for more thinking than the
/// total output room). When the cap is unset, the budget passes through
/// (the caller is expected to have set a cap; unclamped is honest about
/// what was configured rather than inventing a limit).
fn clamp_thinking_budget(budget: u32, max_output_tokens: Option<u32>) -> u32 {
    match max_output_tokens {
        Some(cap) if cap > 0 => budget.min(cap - 1),
        _ => budget,
    }
}

/// Convert input items to OpenAI chat message JSON values.
fn map_input_item(item: &InputItem) -> Value {
    match item {
        InputItem::User { content } => json!({"role": "user", "content": content}),
        InputItem::Assistant {
            content,
            tool_calls,
        } => {
            let mut msg = json!({"role": "assistant", "content": content});
            if !tool_calls.is_empty() {
                let tcs: Vec<Value> = tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": c.input.to_string(),
                            }
                        })
                    })
                    .collect();
                msg["tool_calls"] = Value::Array(tcs);
            }
            msg
        }
        InputItem::ToolResult { call_id, output } => json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": output.to_string(),
        }),
    }
}

/// Build the chat/completions JSON body from the unified CompletionRequest.
fn build_request_body(req: &CompletionRequest) -> Value {
    let marks = |kind: BreakpointKind| req.cache_breakpoints.iter().any(|bp| bp.kind == kind);
    // The whole instructions string, static prefix and cwd-dependent suffix
    // alike, rides in one leading system message, so the tools and history
    // render after it. A cwd change thus diverges the contiguous prefix inside
    // the system content and the identical tools past it do not reuse; only the
    // static segment before the boundary survives. Moving the suffix below the
    // tools would recover them, but it carries the repo's engineering rules at
    // system priority and demoting them trades a safety guarantee for a rare
    // cross-cwd win. Within a session the cwd is fixed, the system message is
    // byte-stable turn to turn, and the full prefix reuses, so this order costs
    // nothing on the common path.
    let mut messages = Vec::with_capacity(req.input.len() + 1);
    let marked = req.cache_format == PromptCacheFormat::PositionalMarkers
        && marks(BreakpointKind::SystemStaticPrefix);
    messages.push(system_message(&req.instructions, marked));
    messages.extend(req.input.iter().map(map_input_item));
    let mut body = json!({"model": req.model, "messages": messages, "stream": false});
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    if let Some(max) = req.settings.max_output_tokens {
        body["max_tokens"] = json!(max);
    }
    // Effort parameters by dialect: qwen3 gets thinking flag + budget (budget
    // only when thinking is on — sending a budget alongside enable_thinking:
    // false is a contradictory request), OpenAI reasoning gets
    // reasoning_effort, everything else gets nothing. Mutual exclusion is
    // structural: a qwen3 model never emits reasoning_effort and vice versa,
    // so a misconfigured settings struct cannot cross the streams.
    match effort_dialect(&req.model) {
        EffortDialect::Qwen3 => {
            if let Some(flag) = req.settings.enable_thinking {
                body["enable_thinking"] = json!(flag);
            }
            // A budget alongside enable_thinking: false is a contradictory
            // request; suppress it when thinking is off (Low). Unspecified
            // defaults to on (the caller opted into a thinking model).
            let thinking_on = req.settings.enable_thinking.unwrap_or(true);
            if let Some(budget) = req.settings.thinking_budget.filter(|_| thinking_on) {
                let clamped = clamp_thinking_budget(budget, req.settings.max_output_tokens);
                body["thinking_budget"] = json!(clamped);
            }
        }
        EffortDialect::OpenaiReasoning | EffortDialect::Glm => {
            if let Some(effort) = req.settings.reasoning_effort {
                body["reasoning_effort"] = json!(effort_str(effort));
            }
        }
        EffortDialect::NotSupported => {}
    }
    if let Some(t) = req.settings.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = req.settings.top_p {
        body["top_p"] = json!(p);
    }
    // Fast mode lowers to the provider's service_tier wire string here — the
    // only place the raw string exists. Set only when the caller pinned Fast
    // (the apply path gates this on catalog-declared model support); absent
    // leaves the project default tier in place.
    if req.settings.speed == Some(SpeedMode::Fast) {
        body["service_tier"] = json!("fast");
    }
    // The declared form picks the convention. A marker-bearing endpoint reuses
    // only what a marker covers, so each breakpoint the policy placed goes at
    // its own position; an automatic-prefix endpoint caches the leading prefix
    // itself and needs only the label.
    match req.cache_format {
        PromptCacheFormat::PositionalMarkers => {
            if marks(BreakpointKind::LastToolDef) {
                mark_last_tool(&mut body);
            }
            if marks(BreakpointKind::LatestUserMessage) {
                mark_tail_message(&mut body["messages"]);
            }
        }
        PromptCacheFormat::AutoPrefix => {
            if let Some(key) = lower_prompt_cache_key(req) {
                body["prompt_cache_key"] = json!(key);
            }
        }
    }
    body
}

/// The leading system message: the instructions as one marked text part, or
/// as a plain string when the endpoint takes no marker here. An empty prompt
/// keeps the plain form too: a marker over nothing covers nothing.
fn system_message(instructions: &str, marked: bool) -> Value {
    if marked && !instructions.is_empty() {
        json!({"role": "system", "content": marked_text(instructions)})
    } else {
        json!({"role": "system", "content": instructions})
    }
}

/// A text payload in the marker-bearing content form.
fn marked_text(text: &str) -> Value {
    json!([{
        "type": "text",
        "text": text,
        "cache_control": cache_marker()
    }])
}

/// The breakpoint value itself. Both marker positions carry the same
/// ephemeral form, so the wire value lives in one place.
fn cache_marker() -> Value {
    json!({"type": "ephemeral"})
}

/// Attach the marker to the last tool definition, beside its type and
/// function: the tool schemas are the largest stable block after the system
/// prefix.
fn mark_last_tool(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = cache_marker();
    }
}

/// Attach the marker to the newest message carrying text, where the sliding
/// reuse point sits this turn. An assistant message holding only tool calls
/// has no text, so the scan moves past it.
fn mark_tail_message(messages: &mut Value) {
    let Some(list) = messages.as_array_mut() else {
        return;
    };
    for message in list.iter_mut().rev() {
        let conversational = matches!(
            message.get("role").and_then(Value::as_str),
            Some("user") | Some("assistant") | Some("tool")
        );
        if conversational && mark_message_text(message) {
            return;
        }
    }
}

/// Rewrap a message's text content in the marker-bearing form; false when
/// there is no text to mark.
fn mark_message_text(message: &mut Value) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    match content.as_str() {
        Some(text) if !text.is_empty() => {
            let text = text.to_string();
            *content = marked_text(&text);
            true
        }
        _ => false,
    }
}

/// The label an automatic-prefix endpoint reuses a cached prefix under: a hex
/// hash of the static segment of the instructions, everything before
/// dynamic_boundary, plus the tool names, so a working-directory change keeps
/// one label for the session. None when no breakpoint asks for one (the None
/// policy, or a provider that skips hints). The hash is deterministic, so a
/// changed static segment or tool set rotates the label into a deliberate miss.
fn lower_prompt_cache_key(req: &CompletionRequest) -> Option<String> {
    let has_prefix_breakpoint = req
        .cache_breakpoints
        .iter()
        .any(|bp| bp.kind == BreakpointKind::SystemStaticPrefix);
    if !has_prefix_breakpoint {
        return None;
    }
    // Hash the static segment only. An offset that was never threaded falls
    // back to the whole instructions, which is the behavior from before the
    // offset existed. get(..offset) also yields None for an offset that is out
    // of range or not a char boundary, so the same fallback keeps the slice
    // from panicking on a malformed offset.
    let boundary = req.dynamic_boundary.unwrap_or(req.instructions.len());
    let static_segment = req
        .instructions
        .get(..boundary)
        .unwrap_or(&req.instructions);
    let mut hasher = DefaultHasher::new();
    static_segment.hash(&mut hasher);
    req.tools.iter().for_each(|t| t.name.hash(&mut hasher));
    Some(format!("houyi-{:016x}", hasher.finish()))
}

/// Parse a non-streaming chat/completions response into CompletionResponse.
/// Tool-call arguments is a JSON string from the provider; parse it to a
/// Value, falling back to {} on parse failure (the model emitted malformed
/// JSON — the tool layer will surface the error rather than crashing the loop).
fn parse_response(json: &Value, model: &str) -> Result<CompletionResponse, ProviderError> {
    let choice = json
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| ProviderError::Unknown("response has no choices".into()))?;
    let msg = choice
        .get("message")
        .ok_or_else(|| ProviderError::Unknown("choice has no message".into()))?;
    let mut output = Vec::new();
    if let Some(content) = msg.get("content").and_then(|v| v.as_str())
        && !content.is_empty()
    {
        output.push(OutputItem::Text {
            text: content.to_string(),
        });
    }
    let mut unique_id = unique_id_gen();
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tool_calls {
            let raw_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let id = unique_id(raw_id);
            let func = tc.get("function").unwrap_or(&Value::Null);
            let name = func
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args_str = func
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
            output.push(OutputItem::ToolCall { id, name, input });
        }
    }
    let usage = parse_usage(json.get("usage"));
    Ok(CompletionResponse {
        output,
        usage,
        model: model.to_string(),
    })
}

#[cfg(test)]
mod tests;
