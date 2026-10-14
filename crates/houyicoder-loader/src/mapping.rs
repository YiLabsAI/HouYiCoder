//! Mapping from a session transcript's typed events to SessionLogEntries. The
//! source jsonl is a flat stream of typed records
//! (user / assistant / system / mode / ...), chained by parentUuid (UUID
//! linkage). The engine's chain is prev_hash (SHA-256 of the previous line's
//! serde bytes). The converter rebuilds a fresh hash chain, so the output
//! verifies under the disk-chain check; the source schema is not
//! preserved (Unverified source, self-consistent rebuilt durable chain).
//!
//! Mapping (source record to SessionEvent):
//!   user (isMeta=false)            -> UserInput
//!   user (isMeta=true)             -> MetaUser
//!   assistant text block           -> folded into AssistantMessage
//!   assistant thinking block       -> Reasoning (raw, replay fidelity)
//!   assistant tool_use block       -> ToolCall
//!   user tool_result block         -> ToolResult
//!   mode / permission-mode / system / attachment / file-history-* / etc.
//!     -> skipped (not needed to resume the readable transcript)

use chrono::DateTime;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use serde_json::Value;

/// Convert one source record into zero or more houyi SessionLogEntries. An assistant
/// record can yield several (Reasoning + ToolCall + AssistantMessage); a user
/// record with tool_result blocks yields several ToolResults; a skipped
/// record yields none. ts_ms is the record's timestamp parsed to unix
/// milliseconds (the caller parses once + passes it in so each mapped event
/// shares the record's time).
pub(crate) fn map_record(
    rec: &Value,
    sid: SessionId,
    ts_ms: u64,
    model_out: &mut Option<String>,
    cwd_out: &mut Option<String>,
) -> Vec<SessionLogEntry> {
    let ty = rec.get("type").and_then(Value::as_str).unwrap_or("");
    // Session origin fields appear on every record; capture the first seen.
    if model_out.is_none()
        && let Some(m) = rec
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
    {
        *model_out = Some(m.to_string());
    }
    if cwd_out.is_none()
        && let Some(c) = rec.get("cwd").and_then(Value::as_str)
    {
        *cwd_out = Some(c.to_string());
    }

    let mut out: Vec<SessionLogEntry> = Vec::new();
    match ty {
        "user" => map_user(rec, sid, ts_ms, &mut out),
        "assistant" => map_assistant(rec, sid, ts_ms, &mut out),
        _ => {} // skipped: system / mode / permission-mode / attachment / ...
    }
    out
}

/// A user record. The content is either a plain string (a human prompt) or a
/// block array (tool_result blocks returning tool output, or text). isMeta
/// marks injected control messages (e.g. the resume-directly nudge) -> map to
/// MetaUser, its own event rather than a user prompt: model input reads it as
/// a directive, and it does not ride the frontend user-message stream.
fn map_user(rec: &Value, sid: SessionId, ts_ms: u64, out: &mut Vec<SessionLogEntry>) {
    let is_meta = rec.get("isMeta").and_then(Value::as_bool).unwrap_or(false);
    let Some(content) = rec.get("message").and_then(|m| m.get("content")) else {
        return;
    };
    match content {
        Value::String(s) => push(out, sid, ts_ms, user_kind(s, is_meta)),
        Value::Array(blocks) => {
            let mut text_buf = String::new();
            for b in blocks {
                if let Some(obj) = b.as_object() {
                    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
                        "tool_result" => {
                            let call_id = obj
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let output = obj.get("content").cloned().unwrap_or(Value::Null);
                            push(
                                out,
                                sid,
                                ts_ms,
                                SessionEvent::ToolResult {
                                    call_id,
                                    output,
                                    duration_ms: 0,
                                },
                            );
                        }
                        "text" => {
                            if let Some(t) = obj.get("text").and_then(Value::as_str) {
                                text_buf.push_str(t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !text_buf.is_empty() {
                push(out, sid, ts_ms, user_kind(&text_buf, is_meta));
            }
        }
        _ => {}
    }
}

fn user_kind(text: &str, is_meta: bool) -> SessionEvent {
    if is_meta {
        SessionEvent::MetaUser {
            text: text.to_string(),
        }
    } else {
        SessionEvent::UserInput {
            text: text.to_string(),
        }
    }
}

/// An assistant record: iterate content blocks. A thinking block emits a
/// Reasoning event (raw, for replay fidelity) and folds into the
/// AssistantMessage.thinking field. A tool_use block emits a ToolCall. A
/// text block accumulates into the AssistantMessage.text. One
/// AssistantMessage is emitted per record (after all blocks), carrying the
/// accumulated text and the folded thinking.
fn map_assistant(rec: &Value, sid: SessionId, ts_ms: u64, out: &mut Vec<SessionLogEntry>) {
    let Some(content) = rec
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let mut text_buf = String::new();
    let mut thinking_buf = String::new();
    for b in content {
        let Some(obj) = b.as_object() else { continue };
        match obj.get("type").and_then(Value::as_str).unwrap_or("") {
            "thinking" => {
                if let Some(t) = obj.get("thinking").and_then(Value::as_str) {
                    push(
                        out,
                        sid,
                        ts_ms,
                        SessionEvent::Reasoning {
                            text: t.to_string(),
                        },
                    );
                    thinking_buf.push_str(t);
                }
            }
            "tool_use" => {
                let call_id = obj
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input = obj.get("input").cloned().unwrap_or(Value::Null);
                push(
                    out,
                    sid,
                    ts_ms,
                    SessionEvent::ToolCall {
                        call_id,
                        tool,
                        input,
                    },
                );
            }
            "text" => {
                if let Some(t) = obj.get("text").and_then(Value::as_str) {
                    text_buf.push_str(t);
                }
            }
            _ => {}
        }
    }
    if !text_buf.is_empty() {
        push(
            out,
            sid,
            ts_ms,
            SessionEvent::AssistantMessage {
                text: text_buf,
                thinking: if thinking_buf.is_empty() {
                    None
                } else {
                    Some(thinking_buf)
                },
            },
        );
    }
}

fn push(out: &mut Vec<SessionLogEntry>, sid: SessionId, ts_ms: u64, kind: SessionEvent) {
    out.push(SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: ts_ms,
        prev_hash: None, // set by the writer when chaining
        event: kind,
    });
}

/// Parse a timestamp (ISO 8601 / RFC 3339) to unix milliseconds.
/// Falls back to 0 on any parse failure (the chain is byte-stable, not
/// time-stable; a zero ts still renders).
pub(crate) fn parse_ts_ms(ts: &str) -> u64 {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp_millis().max(0) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
