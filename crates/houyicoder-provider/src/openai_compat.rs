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

use crate::http_error::{classify_with_body, map_reqwest_err, parse_retry_after};
use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_protocol::cache_policy::BreakpointKind;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, EffortLevel, InputItem, LlmEvent, ModelCapabilities,
    OutputItem, ProviderError, Usage,
};
use serde_json::{Value, json};

/// An OpenAI-compatible chat/completions provider. Configure with base_url
/// (e.g. https://api.openai.com/v1) and api_key; the model is per-request.
/// from_env delegates to the config layer for the unified env resolution.
pub struct OpenAiCompatibleProvider {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl OpenAiCompatibleProvider {
    /// Construct a provider over an OpenAI-compatible endpoint.
    pub fn new(base_url: String, api_key: String) -> Self {
        let http = reqwest::Client::builder()
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

    /// Construct from env via the config layer (the single resolution point):
    /// DASHSCOPE_API_KEY > OPENAI_API_KEY > HOUYICODER_API_KEY for the key;
    /// DASHSCOPE_BASE_URL > OPENAI_BASE_URL > DEFAULT_BASE_URL for the URL.
    /// Returns Auth when no key env var is set (fail-closed).
    pub fn from_env() -> Result<Self, ProviderError> {
        let api_key = houyicoder_config::resolve_api_key().ok_or(ProviderError::Auth)?;
        let base_url = houyicoder_config::resolve_base_url();
        Ok(Self::new(base_url, api_key))
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
        cache_path: std::path::PathBuf,
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
            crate::served_models::write_cache(&cache_path, &ids);
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

    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    #[expect(unused_assignments, reason = "default-then-overwrite")]
    fn stream(
        &self,
        req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>>
    {
        use futures::StreamExt;

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
            let mut final_usage: Option<houyicoder_protocol::llm::Usage> = None;
            let mut tool_calls: Vec<ToolCallAccum> = Vec::new();
            // Track which stream is open so Start/End pair correctly. A
            // reasoning model (Qwen3 with enable_thinking) streams
            // delta.reasoning_content BEFORE delta.content; a non-reasoning
            // model emits only content. TextStart is emitted lazily on the
            // first content delta (not eagerly) so a reasoning-only or
            // tool-only stream does not produce a spurious empty text block.
            let mut text_started = false;
            let mut reasoning_started = false;
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
                        // Break BOTH loops → close-gracefully path. A plain
                        // break only left the line-parse loop, so the outer
                        // chunk loop read one more empty chunk before
                        // terminating.
                        break 'stream;
                    }
                    let json: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    // Extract usage if present (final chunk). Routed through
                    // parse_usage (not serde from_value) so the nested
                    // completion_tokens_details/prompt_tokens_details breakdown
                    // is read consistently with the non-streaming path.
                    if let Some(usage) = json.get("usage")
                        && !usage.is_null()
                    {
                        final_usage = Some(parse_usage(Some(usage)));
                    }
                    let choices = match json.get("choices").and_then(|c| c.as_array()) {
                        Some(c) => c,
                        None => continue,
                    };
                    let choice = match choices.first() {
                        Some(c) => c,
                        None => continue,
                    };
                    let delta = match choice.get("delta") {
                        Some(d) => d,
                        None => continue,
                    };
                    // Reasoning content (Qwen3/DashScope thinking field). The
                    // field name follows the DeepSeek convention; verified
                    // from Qwen-Agent oai.py. Without this a thinking model's
                    // reasoning phase produces no content deltas, so the
                    // token meter reads 0 and the thinking row never shows.
                    if let Some(reasoning) = delta
                        .get("reasoning_content")
                        .and_then(|c| c.as_str())
                        && !reasoning.is_empty()
                    {
                        if !reasoning_started {
                            reasoning_started = true;
                            yield Ok(LlmEvent::ReasoningStart {
                                id: "reason-0".into(),
                            });
                        }
                        yield Ok(LlmEvent::ReasoningDelta {
                            id: "reason-0".into(),
                            text: reasoning.into(),
                        });
                    }
                    if let Some(content) = delta.get("content").and_then(|c| c.as_str())
                        && !content.is_empty()
                    {
                        // Transition reasoning → text: close reasoning first.
                        if reasoning_started {
                            reasoning_started = false;
                            yield Ok(LlmEvent::ReasoningEnd {
                                id: "reason-0".into(),
                            });
                        }
                        if !text_started {
                            text_started = true;
                            yield Ok(LlmEvent::TextStart { id: "text-0".into() });
                        }
                        yield Ok(LlmEvent::TextDelta {
                            id: "text-0".into(),
                            text: content.into(),
                        });
                    }
                    // Accumulate streamed tool-call fragments by index.
                    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            accumulate_tool_call(&mut tool_calls, tc);
                        }
                    }
                    if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                        if reasoning_started {
                            reasoning_started = false;
                            yield Ok(LlmEvent::ReasoningEnd {
                                id: "reason-0".into(),
                            });
                        }
                        if text_started {
                            yield Ok(LlmEvent::TextEnd { id: "text-0".into() });
                        }
                        // Emit the reassembled tool calls so the loop can
                        // dispatch them.
                        for ev in finalize_tool_calls(std::mem::take(&mut tool_calls)) {
                            yield Ok(ev);
                        }
                        yield Ok(LlmEvent::StepFinish {
                            index: 0,
                            reason: reason.into(),
                            usage: final_usage.clone(),
                        });
                        yield Ok(LlmEvent::Finish {
                            reason: reason.into(),
                            usage: final_usage.clone(),
                        });
                        return;
                    }
                }
            }
            // Stream ended without [DONE] or finish_reason — an abnormal
            // close. Delegate to the pure helper so the close-gracefully
            // decision (length when text was in flight, else stop) and the
            // closing event sequence are unit-testable without a mock SSE
            // server (this stream body is HTTP-bound).
            for ev in abnormal_close_events(
                reasoning_started,
                text_started,
                std::mem::take(&mut tool_calls),
                final_usage,
            ) {
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
/// finish so the loop can dispatch the call (live tool-input display is Phase 1).
#[derive(Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    args: String,
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
fn abnormal_close_events(
    reasoning_started: bool,
    text_started: bool,
    tool_calls: Vec<ToolCallAccum>,
    final_usage: Option<houyicoder_protocol::llm::Usage>,
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
fn accumulate_tool_call(acc: &mut Vec<ToolCallAccum>, tc: &Value) {
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

/// A per-response generator of unique tool-call ids. A raw id that is empty
/// or already seen within this response (some OpenAI-compatible endpoints
/// omit the id field on tool_call deltas, or reuse one id across calls in a
/// single message) is replaced with a minted houyi_tc_N. Pairing a call to
/// its result downstream keys on call_id, so a duplicate id crosses results
/// across calls. Uniqueness is established here at the provider boundary,
/// once, so every consumer below pairs by identity rather than by arrival:
///
/// - transcript FIFO: take_update pairs the next matching id
/// - pending_approvals: a HashSet of call_id marks a call answered by id
/// - apply_decisions: routes a decision by find on call_id
/// - model history: tool messages echo tool_call_id
///
/// A duplicate id is not only a display bug: pending_approvals would mark
/// every same-id call answered (silently dropping a pending call) and
/// apply_decisions could route an approval onto the wrong call — a safety,
/// not just a display, failure. The mint makes it unreachable; the consumers
/// above do not re-defend.
///
/// The counter is process-global; uniqueness holds within one process
/// lifetime. Two same-class assumptions stand, neither defended here: a raw
/// id will not literally match the minted prefix houyi_tc_N (a provider
/// echoing our own minted id back would let the counter later mint a
/// colliding one — the generator does not re-check seen after minting), and
/// uniqueness is per-process (if a transcript-frame rehydration path is ever
/// added, frames rebuilt from a persisted log in a new process, the mint
/// must gain a session prefix so a fresh counter cannot collide with
/// persisted ids).
fn unique_id_gen() -> impl FnMut(&str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    move |raw: &str| {
        if raw.is_empty() || seen.contains(raw) {
            let id = format!(
                "houyi_tc_{}",
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
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
fn finalize_tool_calls(acc: Vec<ToolCallAccum>) -> Vec<LlmEvent> {
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
    /// qwen3 family: enable_thinking + thinking_budget.
    Qwen3,
    /// o1/o3/gpt-5 family: reasoning_effort.
    OpenaiReasoning,
    /// Neither family matched: no effort parameters sent.
    NotSupported,
}

/// Probe the model id for its effort dialect. qwen3 wins over the OpenAI
/// reasoning family (a hypothetical qwen3-reasoning id is qwen3 first). The
/// OpenAI reasoning arm matches o1, o3, and gpt-5 substrings case-insensitive.
pub fn effort_dialect(model: &str) -> EffortDialect {
    let m = model.to_lowercase();
    if m.contains("qwen3") {
        EffortDialect::Qwen3
    } else if m.contains("o1") || m.contains("o3") || m.contains("gpt-5") {
        EffortDialect::OpenaiReasoning
    } else {
        EffortDialect::NotSupported
    }
}

/// The wire string for an effort level (lowercase, matching the serde form).
fn effort_str(effort: EffortLevel) -> &'static str {
    match effort {
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        EffortLevel::High => "high",
    }
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

/// Build the chat/completions JSON body from the unified CompletionRequest.
/// Assistant tool_calls are emitted as OpenAI tool_calls (arguments is a JSON
/// string, per the OpenAI spec — providers parse it back). ToolResult maps
/// to the tool role with tool_call_id.
fn build_request_body(req: &CompletionRequest) -> Value {
    let mut messages = Vec::with_capacity(req.input.len() + 1);
    messages.push(json!({"role": "system", "content": req.instructions}));
    for item in &req.input {
        match item {
            InputItem::User { content } => {
                messages.push(json!({"role": "user", "content": content}));
            }
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
                messages.push(msg);
            }
            InputItem::ToolResult { call_id, output } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output.to_string(),
                }));
            }
        }
    }
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
        EffortDialect::OpenaiReasoning => {
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
    // Lower the symbolic cache breakpoints to OpenAI's prompt_cache_key: a
    // single stable label for the cached prefix (system + tools). OpenAI
    // auto-caches the leading prefix; the key labels it so identical prefixes
    // reuse the cache across requests. The other breakpoint kinds (LastToolDef,
    // LatestUserMessage) have no single-key equivalent here — auto-cache
    // handles the sliding reuse. A provider with positional cache_control
    // blocks lowers each kind to its own position instead.
    if let Some(key) = lower_prompt_cache_key(req) {
        body["prompt_cache_key"] = json!(key);
    }
    body
}

/// Derive the OpenAI prompt_cache_key from the request's symbolic breakpoints.
/// When the Auto policy placed a SystemStaticPrefix breakpoint, hash the stable
/// prefix (instructions + serialized tools) into a short hex label. Returns
/// None when no breakpoint asks for a cache key (the None policy, or a
/// provider that skips hints). The hash is deterministic so the same prefix
/// reuses the cache across turns; a changed system prompt or tool set produces
/// a new key + a deliberate miss.
fn lower_prompt_cache_key(req: &CompletionRequest) -> Option<String> {
    let has_prefix_breakpoint = req
        .cache_breakpoints
        .iter()
        .any(|bp| bp.kind == BreakpointKind::SystemStaticPrefix);
    if !has_prefix_breakpoint {
        return None;
    }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    req.instructions.hash(&mut hasher);
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

/// Map the OpenAI usage object to the inclusive-totals Usage struct. OpenAI
/// reports inclusive prompt_tokens (no cache breakdown unless usage_cache
/// is requested) — records the totals and leaves the breakdown at zero.
/// When streaming lands, the per-provider mapper picks the add-vs-subtract
/// semantic (OpenAI subtracts cached; some compatible vendors add
/// inclusive).
fn parse_usage(u: Option<&Value>) -> Usage {
    let Some(u) = u else {
        return Usage::default();
    };
    fn get(u: &Value, k: &str) -> u32 {
        u.get(k).and_then(|v| v.as_u64()).unwrap_or(0) as u32
    }
    let input_tokens = get(u, "prompt_tokens");
    let output_tokens = get(u, "completion_tokens");
    let total_tokens = get(u, "total_tokens");
    // Thinking models on OpenAI-compat providers (Qwen3/DashScope) nest the
    // reasoning-token count under completion_tokens_details, and the cached
    // prefix under prompt_tokens_details.cached_tokens. Both are subsets:
    // reasoning of output, cached of input. Without this the reasoning budget
    // reads 0 and the cache-read column never fills. The OpenAI-compat
    // API nests these fields (reasoning under completion_tokens_details,
    // cached under prompt_tokens_details) — this extraction unpacks them.
    let details = u.get("completion_tokens_details").unwrap_or(&Value::Null);
    let prompt_details = u.get("prompt_tokens_details").unwrap_or(&Value::Null);
    let reasoning_tokens = get(details, "reasoning_tokens");
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

#[cfg(test)]
#[path = "openai_compat_tests.rs"]
mod tests;
