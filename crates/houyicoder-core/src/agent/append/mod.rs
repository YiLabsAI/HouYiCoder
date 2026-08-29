//! Session-log append helpers for Runner. Each Runner method here writes a
//! specific event kind to the lossless event log; new_event stamps a fresh id +
//! wall clock before the store sets prev_hash on append.

mod hook;
pub(crate) use hook::{emit_live_line, record_hook_signals};

use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_api::live::{LiveEvent, LiveSink};
use houyicoder_context::{ContextBackend, EventId, SessionId, TurnEvent, TurnEventKind};
use houyicoder_protocol::llm::{OutputItem, Usage};
use serde_json::Value;

use super::hook::{HookEvent, wire::HookOutcome};
use super::{CompletionResponse, RunError, Runner};

/// Forward a turn-boundary snapshot to the live sink. Called after a turn
/// resolves and the loop will run again; the snapshot carries the turn's own
/// tool count, the last tool name, and cumulative tokens so a watcher that
/// does not consume the token stream (a spawned child's bus bridge) can
/// report per-turn progress. No-op when no sink is installed.
pub(crate) fn emit_turn_progress(
    live: Option<&LiveSink>,
    response: &CompletionResponse,
    turn: u32,
    usage: &Usage,
) {
    let Some(sink) = live else {
        return;
    };
    let tool_uses = response
        .output
        .iter()
        .filter(|item| matches!(item, OutputItem::ToolCall { .. }))
        .count() as u32;
    let last_activity = response.output.iter().rev().find_map(|item| match item {
        OutputItem::ToolCall { name, .. } => Some(name.clone()),
        _ => None,
    });
    sink(&LiveEvent::TurnBoundary {
        turn,
        cumulative_tokens: usage.input_tokens as u64 + usage.output_tokens as u64,
        tool_uses,
        last_activity,
    });
}

impl Runner {
    /// Surface a notice to the user through the live sink: a transcript
    /// system line the host renders verbatim. For things the user must know
    /// or can act on — a hook that failed, a fence that did not engage. Not
    /// for diagnostics (those go to the tracing sink; the user cannot act
    /// on them and would see noise). No-op when no host installed a sink
    /// (tests, the stub path).
    pub(crate) fn emit_system_line(&self, text: String) {
        emit_live_line(self.live.as_ref(), text);
    }

    /// Notify a watcher the run reached a terminal state. Emitted once at
    /// the end of run(); a spawned child's bus bridge forwards it onto the
    /// child's completed topic. No-op when no sink is installed.
    pub(crate) fn emit_run_completed(&self, status: &str, summary: &str) {
        if let Some(sink) = self.live.as_ref() {
            sink(&LiveEvent::RunCompleted {
                status: status.to_string(),
                summary: summary.to_string(),
            });
        }
    }
    /// Surface the one overflow case the catalog cannot self-heal: the
    /// provider rejected an over-long request but its error body carried no
    /// parseable limit, so record_learned_context_window learned nothing. The
    /// notice points the user at the catalog override rather than guessing.
    /// Dedup-per-(model,session) is a follow-up; the case is rare.
    pub(crate) fn emit_unactionable_overflow(&self, model: &str) {
        self.emit_system_line(format!(
            "{model} was rejected as too long but the provider did not report its limit; set model.catalog[{model}].context_window to this model's real window",
        ));
    }

    /// Append a user input event to the log.
    pub(crate) async fn append_user_input(
        &self,
        session: SessionId,
        text: String,
    ) -> Result<(), RunError> {
        self.store
            .append(new_event(session, TurnEventKind::UserInput { text }))
            .await?;
        Ok(())
    }

    /// Drain the bus inbox receiver (a spawned child's steering channel)
    /// into the texts the drive loop appends as user messages this turn.
    /// Empty for the top-level runner, which has no inbox. Non-blocking.
    pub(crate) fn drain_inbox(&self) -> Vec<String> {
        let mut guard = self.inbox.lock().expect("inbox lock");
        let Some(rx) = guard.as_mut() else {
            return Vec::new();
        };
        let mut acc = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let crate::agent::multi_agent::bus_types::BusMessage::Inbox { text } = msg {
                acc.push(text);
            }
        }
        acc
    }

    /// Append a queued interjection: a user message the human queued while a
    /// run was in flight, drained + appended at the next turn boundary. The
    /// model-input projection wraps it with a framing note so the model reads
    /// it as a mid-work interjection (continue the task + address), not a
    /// fresh instruction that drops the in-flight task. The durable text is
    /// the bare user input (the transcript shows it verbatim).
    pub(crate) async fn append_mid_turn_input(
        &self,
        session: SessionId,
        text: String,
    ) -> Result<(), RunError> {
        self.store
            .append(new_event(session, TurnEventKind::MidTurnInput { text }))
            .await?;
        Ok(())
    }

    /// Append a child-completion notification: a background subagent finished
    /// and the parent model must learn of the result at this turn boundary.
    /// Distinct from append_mid_turn_input (a user interjection) so the
    /// durable log + the projection distinguish a child result from a user
    /// message. The projection frames it as a background result, not a
    /// continue-the-task note. turn + order are best-effort (the event-log
    /// order is authoritative, as with SubagentReturn).
    pub(crate) async fn append_notification(
        &self,
        session: SessionId,
        child_session_id: String,
        text: String,
    ) -> Result<(), RunError> {
        self.store
            .append(new_event(
                session,
                TurnEventKind::NotificationInjected {
                    child_session_id,
                    turn: 0,
                    order: 0,
                    topic: "completion".to_string(),
                    summary: text,
                },
            ))
            .await?;
        Ok(())
    }

    /// Observe a batch of to-be-executed calls for redundancy, then append a
    /// dedup reminder for each newly-flagged duplicate as a persisted MetaUser
    /// event so the next turn's projection serves it to the model as a
    /// system-reminder. This is the instant-feedback half of the reward loop
    /// (the delayed half is the dream distilling lessons from the same
    /// signal). Best-effort: a failed append logs and never fails the run
    /// (redundancy is an observer, not a gate). prev_records is the records
    /// length before check_batch so only the newly-flagged calls this batch
    /// get a reminder.
    pub(crate) async fn observe_redundancy(
        &self,
        session: SessionId,
        calls: &[(&str, &serde_json::Value)],
    ) {
        if std::env::var("HOUYICODER_REWARD_OFF").is_ok() {
            return;
        }
        let (prev_flagged, prev_retry, prev_retry_tools) = {
            let t = self.redundancy.lock().expect("redundancy");
            (
                t.flagged_total(),
                t.retry_after_error(),
                t.retry_tools().len(),
            )
        };
        if let Ok(mut t) = self.redundancy.lock() {
            t.check_batch(calls.iter().copied());
        }
        let (new_redundant, new_retry, tools, retry_tools) = {
            let t = self.redundancy.lock().expect("redundancy");
            let nr = (t.flagged_total() - prev_flagged) as u32;
            let nretry = t.retry_after_error() - prev_retry;
            // The ring is capped at 256; once full, skip(prev_records) is
            // always skip(256) = empty. Use the ring tail (the most recent
            // nr items) instead, which is correct regardless of cap state.
            let rec_len = t.records().len();
            let take = (nr as usize).min(rec_len);
            let start = rec_len.saturating_sub(take);
            let tools: Vec<String> = t
                .records()
                .iter()
                .skip(start)
                .map(|r| r.tool.clone())
                .collect();
            let retry_tools: Vec<String> = t
                .retry_tools()
                .iter()
                .skip(prev_retry_tools)
                .cloned()
                .collect();
            (nr, nretry, tools, retry_tools)
        };
        // Durable reward observation: a later dream can scan cross-session
        // reward trends from the trajectory log (the in-memory tracker is
        // process-scoped and lost on exit). Only appended when this batch
        // flagged something, so a quiet turn writes nothing.
        if (new_redundant > 0 || new_retry > 0)
            && let Err(e) = self
                .store
                .append(new_event(
                    session,
                    TurnEventKind::RewardObservation {
                        redundant: new_redundant,
                        retry_after_error: new_retry,
                    },
                ))
                .await
        {
            tracing::warn!("reward observation append failed: {e}");
        }
        for tool in &tools {
            let text = format!(
                "Note: you just called {tool} with the same input earlier; the prior result is still valid. Reuse it instead of calling again — re-issuing the same call wastes tokens without new information."
            );
            if let Err(e) = self
                .store
                .append(new_event(session, TurnEventKind::MetaUser { text }))
                .await
            {
                tracing::warn!("redundancy reminder append failed: {e}");
            }
        }
        // Blind-retry warning: the agent re-issued a call that already failed
        // with the same input, without changing anything. Unlike the redundant-
        // call reminder (the prior result is still valid), the prior result
        // was an error — re-running it will likely produce the same error.
        // This real-time nudge lets the agent course-correct within the
        // current query, not just in the next query after the dream writes a
        // lesson.
        for tool in &retry_tools {
            let text = format!(
                "Note: you just re-issued {tool} with the same input that already failed, without \
                 changing anything between attempts. This is a blind retry — the result will \
                 likely be the same error. Diagnose the prior error output and change your \
                 approach before retrying."
            );
            if let Err(e) = self
                .store
                .append(new_event(session, TurnEventKind::MetaUser { text }))
                .await
            {
                tracing::warn!("blind-retry reminder append failed: {e}");
            }
        }
    }

    /// Append one turn's model response: Reasoning events, then a single
    /// AssistantMessage (concatenated text — anchors the tool calls), then the
    /// ToolCall events. Projection groups AssistantMessage + its ToolCalls.
    ///
    /// The accumulated reasoning text is attached to the AssistantMessage as
    /// the thinking field so a projection has a single-source view of the
    /// reasoning that preceded the message. The raw Reasoning events also
    /// land in the log for replay fidelity.
    ///
    /// fidelity limitation: all Text items are concatenated into one
    /// AssistantMessage and all ToolCalls follow it, so the model's emit order
    /// of interleaved Text/ToolCall blocks is not preserved. InputItem's
    /// Assistant { content: String, tool_calls: Vec } can't represent
    /// interleaving either. Most providers accept the flattened wire form and
    /// the model rarely interleaves meaningfully, so this is a documented
    /// gap — the fix is to widen Assistant to an ordered block list when a real
    /// provider needs it.
    pub(crate) async fn append_response_events(
        &self,
        session: SessionId,
        response: &CompletionResponse,
    ) -> Result<(), RunError> {
        let mut thinking_text = String::new();
        for item in &response.output {
            if let OutputItem::Reasoning { text } = item {
                self.store
                    .append(new_event(
                        session,
                        TurnEventKind::Reasoning { text: text.clone() },
                    ))
                    .await?;
                thinking_text.push_str(text);
            }
        }
        let mut assistant_text = String::new();
        for item in &response.output {
            if let OutputItem::Text { text } = item {
                assistant_text.push_str(text);
            }
        }
        let thinking = if thinking_text.is_empty() {
            None
        } else {
            Some(thinking_text)
        };
        self.store
            .append(new_event(
                session,
                TurnEventKind::AssistantMessage {
                    text: assistant_text,
                    thinking,
                },
            ))
            .await?;
        for item in &response.output {
            if let OutputItem::ToolCall { id, name, input } = item {
                self.store
                    .append(new_event(
                        session,
                        TurnEventKind::ToolCall {
                            call_id: id.clone(),
                            tool: name.clone(),
                            input: input.clone(),
                        },
                    ))
                    .await?;
            }
        }
        Ok(())
    }

    /// Append a tool-result event for the given call id. duration_ms is the
    /// wall-clock length of the tool call this result answers (0 when the
    /// host did not time it — synthetic / interrupted / blocked results carry
    /// 0 since no real execution ran).
    ///
    /// isolate stage (PostToolUse): when the serialized output exceeds
    /// ISOLATE_LARGE_OUTPUT_BYTES, externalize it to the CAS (block_put) and
    /// append a block_ref marker carrying an inline preview instead of the
    /// raw content. The raw stays in the CAS for on-demand materialize; the
    /// served view carries a small pointer. Fail-closed: on no backend or
    /// block_put failure, append the raw output (no content loss).
    pub(crate) async fn append_tool_result(
        &self,
        session: SessionId,
        call_id: String,
        tool: &str,
        output: serde_json::Value,
        duration_ms: u64,
    ) -> Result<(), RunError> {
        let output = self.isolate_large_output(tool, output).await;
        self.store
            .append(new_event(
                session,
                TurnEventKind::ToolResult {
                    call_id,
                    output,
                    duration_ms,
                },
            ))
            .await?;
        Ok(())
    }

    /// Externalize a large tool output to the CAS at the PostToolUse point.
    /// A structured result (a JSON object) externalizes only its largest
    /// top-level string field, so the envelope's other keys (agentId, color,
    /// status, usage) survive inline — the whole-output replacement would
    /// destroy them, and the TUI reads those keys to render the Subagent
    /// fold-group. Blob outputs (no top-level string field, e.g. a grep
    /// matches array) and objects whose largest string field cannot buy
    /// enough headroom fall back to whole-output externalize. Smaller
    /// outputs and any block_put failure pass through unchanged (fail-closed).
    async fn isolate_large_output(
        &self,
        tool: &str,
        output: serde_json::Value,
    ) -> serde_json::Value {
        let bytes = match serde_json::to_vec(&output) {
            Ok(b) => b,
            Err(_) => return output,
        };
        if bytes.len() <= ISOLATE_LARGE_OUTPUT_BYTES {
            return output;
        }
        let backend = self.store.backend();
        // Field-level path: externalize only the largest top-level string
        // field so the envelope's other keys stay inline. The candidate is
        // sized with an upper-bound marker (see marker_upper_bound for the
        // assumption on the reducer).
        if let Some(obj) = output.as_object()
            && let Some((key, field_len)) = largest_string_field(obj)
        {
            let marker_bound = serde_json::to_vec(&marker_upper_bound())
                .map(|b| b.len())
                .unwrap_or(0);
            // The field is no bigger than the marker that would replace
            // it: externalizing buys nothing. Blob outputs (large array,
            // small string fields) land here and keep the whole-output
            // path that grep relies on.
            if field_len > marker_bound {
                let mut candidate = output.clone();
                candidate[key.clone()] = marker_upper_bound();
                let fits = serde_json::to_vec(&candidate)
                    .map(|b| b.len() <= ISOLATE_LARGE_OUTPUT_BYTES)
                    .unwrap_or(false);
                if fits {
                    return self
                        .externalize_field(tool, output, key, Some(backend))
                        .await;
                }
            }
        }
        self.externalize_whole(tool, output, bytes, Some(backend))
            .await
    }

    /// Externalize a single string field of a structured result: block_put
    /// the field's value, replace the field with a marker carrying an inline
    /// preview, keep every other top-level key. Fail-closed: any block_put
    /// or serialization failure returns the original output so no content
    /// is lost.
    async fn externalize_field(
        &self,
        tool: &str,
        output: serde_json::Value,
        key: String,
        backend: Option<&dyn ContextBackend>,
    ) -> serde_json::Value {
        let Some(backend) = backend else {
            return output;
        };
        let field_value = output.get(&key).cloned().unwrap_or(Value::Null);
        let field_bytes = match serde_json::to_vec(&field_value) {
            Ok(b) => b,
            Err(_) => return output,
        };
        let hash = match backend.block_put(field_bytes.clone()).await {
            Ok(h) => h,
            Err(_) => return output,
        };
        // Preview from the raw string content, not the JSON-serialized form,
        // so the fold-group summary shows clean text (real newlines, no
        // escape sequences) instead of "{\"...\\n...\"}".
        let preview_src = field_value
            .as_str()
            .map(|s| s.as_bytes())
            .unwrap_or(&field_bytes);
        let mut out = output;
        out[key] = self.marker_for(tool, &hash.0, preview_src);
        out
    }

    async fn externalize_whole(
        &self,
        tool: &str,
        output: serde_json::Value,
        bytes: Vec<u8>,
        backend: Option<&dyn ContextBackend>,
    ) -> serde_json::Value {
        let Some(backend) = backend else {
            return output;
        };
        let hash = match backend.block_put(bytes.clone()).await {
            Ok(h) => h,
            Err(_) => return output,
        };
        self.marker_for(tool, &hash.0, &bytes)
    }

    /// Build the block_ref marker (preview + hint + data_tag) from the
    /// serialized bytes that were stored. Shared by field-level and
    /// whole-output externalize so the marker vocabulary has one producer.
    fn marker_for(&self, tool: &str, hash: &str, bytes: &[u8]) -> Value {
        let raw_preview = preview_string(bytes);
        let (preview, data_tag) = match &self.reducer {
            Some(r) => {
                let reduced = r.reduce(
                    &raw_preview,
                    tool,
                    &super::reducer::ReduceCtx {
                        raw: false,
                        trust: super::reducer::TrustLevel::Untrusted,
                    },
                );
                (reduced.text, reduced.data_tag)
            }
            None => (raw_preview, false),
        };
        serde_json::json!({
            "block_ref": hash,
            "preview": preview,
            "data_tag": data_tag,
            "hint": "large output compacted; re-invoke the tool to retrieve it",
        })
    }

    /// Drive a manual compaction of the session: replay the event log, fire
    /// PreCompact hooks (whose Inject output becomes custom summarization
    /// instructions), build + commit a CheckpointManifest, then fire
    /// PostCompact. The /compact command calls this. Returns the outcome
    /// (folded count, manifest id, pre/post token estimates) for the wire
    /// reply. The served view picks up the manifest on the next turn's
    /// build_with_manifest — compaction does not reduce the in-flight context
    /// immediately, only the next served window.
    pub async fn compact(
        &self,
        session: SessionId,
    ) -> Result<super::compact::CompactOutcome, RunError> {
        self.compact_internal(session, super::hook::CompactTrigger::Manual)
            .await
    }

    /// Append a logical turn boundary. The durable companion to the in-memory
    /// start_turn: every model-call entry appends one, so a turn that never
    /// reaches a TurnUsage (cancelled / errored mid-stream) still carries a
    /// boundary marker. The trajectory projection groups turns on this, not
    /// on UserInput (a single prompt spans N tool-iteration turns).
    pub(crate) async fn append_turn_started(&self, session: SessionId) -> Result<(), RunError> {
        let (turn, call_in_turn) = match self.observability.lock() {
            Ok(ol) => ol.turn_coords(),
            Err(_) => (0, 0),
        };
        self.store
            .append(new_event(
                session,
                TurnEventKind::TurnStarted { turn, call_in_turn },
            ))
            .await?;
        Ok(())
    }

    /// Append the per-turn usage event: the durable cost record the
    /// trajectory's cost + cache dimensions read on resume, export, and the
    /// self-evolution re-reads. Inline primitive fields (not the provider
    /// Usage type) keep the context crate a serde-only leaf; the mapping
    /// happens here at the append boundary. turn + call_in_turn are read from
    /// the in-memory OL (set by start_turn / record_turn) so the durable
    /// stream carries its own logical-turn grouping — /trajectory renders
    /// a turn with its retry count by grouping on turn instead of deriving boundaries from the
    /// event order. recovery marks a length-recovery continuation so per-call
    /// retry cost is queryable without correlating against TruncationVerdict.
    pub(crate) async fn append_turn_usage(
        &self,
        session: SessionId,
        model: &str,
        usage: &Usage,
        recovery: bool,
        effort: Option<&str>,
    ) -> Result<(), RunError> {
        let (turn, call_in_turn) = match self.observability.lock() {
            Ok(ol) => ol.turn_coords(),
            Err(_) => (0, 0),
        };
        self.store
            .append(new_event(
                session,
                TurnEventKind::TurnUsage {
                    turn,
                    call_in_turn,
                    input_tokens: usage.input_tokens as u64,
                    output_tokens: usage.output_tokens as u64,
                    cache_read_input_tokens: usage.cache_read_input_tokens as u64,
                    cache_write_input_tokens: usage.cache_write_input_tokens as u64,
                    reasoning_tokens: usage.reasoning_tokens as u64,
                    model: model.to_string(),
                    recovery,
                    effort: effort.map(str::to_string),
                },
            ))
            .await?;
        // Cache-break detection: compare cache_read vs the previous turn.
        // A sharp drop (current < prev / 2) is attributed to compact /
        // model-switch / unknown based on flags set since the previous
        // response. Provider-agnostic (runs in the agent loop, not in a
        // provider adapter). Records a durable event (not internal-only
        // telemetry) so /trajectory can surface it.
        let current = usage.cache_read_input_tokens as u64;
        let cause = match self.cache_prev_read.lock() {
            Ok(mut prev) => {
                let p = *prev;
                let cause = if p.is_some_and(|v| v > 0 && current < v / 2) {
                    if self
                        .cache_compact_flag
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        Some("compact")
                    } else if self
                        .cache_model_switch_flag
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        Some("model-switch")
                    } else {
                        Some("unknown")
                    }
                } else {
                    None
                };
                *prev = Some(current);
                cause
            }
            Err(_) => None,
        };
        self.cache_compact_flag
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.cache_model_switch_flag
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(c) = cause {
            self.store
                .append(new_event(
                    session,
                    TurnEventKind::CacheBreak {
                        cause: c.to_string(),
                    },
                ))
                .await?;
        }
        Ok(())
    }

    /// Append one HookSignal per hook outcome (per-hook attribution). Skips
    /// bare Allow — it is derivable from absence (no HookSignal ⟹ every
    /// configured hook allowed). For a hook error the effective verdict comes
    /// from verdict_on_hook_error (the single fail-closed source), so the
    /// durable record can never disagree with what arbitrate actually did.
    /// turn/call_in_turn come from the in-memory OL; tool_name is None for
    /// non-tool events. Best-effort: a store error is logged, not fatal
    /// (hook audit must not crash the run).
    pub(crate) async fn append_hook_signals(
        &self,
        session: SessionId,
        event: HookEvent,
        tool_name: Option<&str>,
        outcomes: &[HookOutcome],
    ) {
        record_hook_signals(
            self.store.as_ref(),
            &self.observability,
            self.live.as_ref(),
            session,
            event,
            tool_name,
            outcomes,
        )
        .await;
    }
}

/// Emit a system line through the live sink when one is attached; no-op
/// Build a TurnEvent with a fresh id and a wall-clock timestamp. prev_hash is
/// set by SessionStore::append.
pub(crate) fn new_event(session: SessionId, kind: TurnEventKind) -> TurnEvent {
    TurnEvent {
        id: EventId::new(),
        session,
        ts: now_ts(),
        prev_hash: None,
        kind,
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Tool outputs larger than this (serialized bytes) are externalized to
/// the CAS at the PostToolUse point. The 8KB threshold balances a single
/// large result against per-turn retrieval cost.
const ISOLATE_LARGE_OUTPUT_BYTES: usize = 8192;

/// Inline preview length, in chars, stored alongside a block_ref marker so
/// the Summarize tier can serve a preview without a retrieval. Matches the
/// retention layer's preview length.
const PREVIEW_CHARS: usize = 200;

/// Build the inline preview string for a serialized output: the first
/// PREVIEW_CHARS chars (UTF-8 lossy) with an ellipsis when truncated.
fn preview_string(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.chars().count() > PREVIEW_CHARS {
        let truncated: String = s.chars().take(PREVIEW_CHARS).collect();
        format!("{truncated}\u{2026}")
    } else {
        s.into_owned()
    }
}

/// The largest top-level string field of a JSON object, by escaped byte
/// length. Returns None when the object has no string field (e.g. a grep
/// result whose payload is a matches array). Used to pick which field to
/// externalize so the envelope's other keys stay inline.
fn largest_string_field(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<(String, usize)> {
    obj.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), json_string_byte_len(s))))
        .max_by_key(|(_, n)| *n)
}

/// Escaped byte length a JSON encoder writes for this string, including the
/// surrounding quotes. Matches serde_json's escaping: " and \ cost 2 bytes,
/// control chars (< 0x20) cost 6 (\uXXXX), everything else its UTF-8 byte
/// count. Used so the field-level decision accounts for escaping, which the
/// whole-output gate measures in the same serialized bytes.
fn json_string_byte_len(s: &str) -> usize {
    2 + s
        .chars()
        .map(|c| match c {
            '"' | '\\' => 2,
            c if (c as u32) < 0x20 => 6,
            _ => c.len_utf8(),
        })
        .sum::<usize>()
}

/// A strict upper bound on the marker object that replaces an externalized
/// field: hash + preview (capped at PREVIEW_CHARS, multi-byte safe via the
/// x4 factor) + hint + data_tag. Used to size-check a candidate envelope
/// before committing to field-level. Assumes the reducer does not expand
/// the preview past this bound; a reducer that annotates or prefixes the
/// preview can exceed it, leaving the inline output over the threshold. The
/// envelope still holds either way (only the inline size is off, and the
/// result is not lost — the raw is in the CAS).
fn marker_upper_bound() -> serde_json::Value {
    serde_json::json!({
        "block_ref": "x".repeat(128),
        "preview": "x".repeat(PREVIEW_CHARS * 4 + 4),
        "data_tag": false,
        "hint": "x".repeat(120),
    })
}
