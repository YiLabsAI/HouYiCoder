//! The /export projection: derives the export JSON document from the durable
//! TurnEvent stream. Sibling to trajectory_bridge — that module owns the
//! /trajectory view projection, this one owns the /export document projection.
//! Both read the same SessionLogTrajectory; the ExportLog impl lives here so
//! trajectory_bridge stays under the file-size gate.
//!
//! The export document is the self-evolution data source: a machine-readable
//! record of everything that happened in the session. Every field is derived
//! from the durable event stream — the session log is the single source of
//! truth, not the in-memory OL aggregator (which may be mid-turn + is not
//! reachable from the TUI bridge). This makes the export stable across
//! resume: the file reflects what the log recorded, not what a live
//! accumulator cached.
//!
//! USD cost is intentionally absent: deriving it needs the pricing table,
//! which the bridge does not hold (the OL owns pricing). Token totals are the
//! durable truth + the cost layer's input; USD lands when the cost-summary
//! save/restore path exposes pricing to the bridge. Per the
//! unknown-must-be-None rule, the missing field is omitted rather than 0.

use std::collections::BTreeMap;

use houyicoder_context::{TurnEvent, TurnEventKind};
use houyicoder_tui::view::export_log::{ExportLog, ExportPayload};

use crate::trajectory_bridge::SessionLogTrajectory;

/// One per-tool aggregate row: call count, failure count, total + max
/// wall-clock latency. Failures are ToolResults whose output carries an
/// error key (the bridge's existing success heuristic). Latency comes from
/// the inline duration_ms on ToolResult (0 when the host did not time the
/// call — synthetic or interrupted results).
#[derive(Debug, serde::Serialize)]
pub struct ToolStat {
    pub tool: String,
    pub calls: u64,
    pub fail: u64,
    pub latency_ms_total: u64,
    pub latency_ms_max: u64,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct TokenCounts {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl TokenCounts {
    fn add(&mut self, u: &TurnEventKind) {
        if let TurnEventKind::TurnUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            ..
        } = u
        {
            self.input_tokens += input_tokens;
            self.output_tokens += output_tokens;
            self.cache_read_input_tokens += cache_read_input_tokens;
            self.cache_write_input_tokens += cache_write_input_tokens;
            self.reasoning_tokens += reasoning_tokens;
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct ModelUsage {
    pub model: String,
    #[serde(flatten)]
    pub counts: TokenCounts,
}

#[derive(Debug, serde::Serialize)]
pub struct UsageSummary {
    pub total: TokenCounts,
    pub per_model: Vec<ModelUsage>,
}

/// A compaction checkpoint or summary line. checkpoint is the CheckpointId
/// (CompactionBoundary); summary is the compressed text (Summary). Both
/// fields are Option because the two event kinds carry different payloads.
#[derive(Debug, serde::Serialize)]
pub struct CheckpointEntry {
    pub ts: u64,
    pub checkpoint: Option<String>,
    pub summary: Option<String>,
}

/// A recorded fault: a hook error (HookSignal with error = Some) or a run
/// abort (TurnAborted). kind discriminates the source; the rest carry the
/// attributable context. Bare-Allow HookSignals are absent by construction
/// (the record layer skips them), so every HookSignal here is a non-Allow
/// verdict — only the error-bearing ones land in errors (the rest are
/// visible in the trajectory stream).
#[derive(Debug, serde::Serialize)]
pub struct ErrorEntry {
    pub ts: u64,
    pub kind: String,
    pub reason: String,
    pub hook_name: Option<String>,
    pub tool_name: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct ExportData {
    pub session_id: String,
    pub model: String,
    /// Wall-clock ms of the first event (session start). 0 for an empty log.
    pub started_at: u64,
    /// The full durable event stream, in append order, with prev_hash chain
    /// intact. This IS the trajectory — the lossless replay substrate.
    pub trajectory: Vec<TurnEvent>,
    pub tool_stats: Vec<ToolStat>,
    pub usage: UsageSummary,
    pub checkpoints: Vec<CheckpointEntry>,
    pub errors: Vec<ErrorEntry>,
}

/// UTC YYYY-MM-DD-HHMM slug for the default filename, from an epoch-ms
/// timestamp. Uses Howard Hinnant's civil_from_days algorithm (public
/// domain) so the export carries no date crate dependency. UTC (not local)
/// on purpose: the export is a durable artifact, stable across timezones.
fn format_ts_slug(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}-{hour:02}{min:02}")
}

/// First user prompt, slugified for a filename: lowercase, non-alphanumeric
/// becomes dash, runs collapsed, trimmed to 40 chars. "session" when no
/// UserInput (the log predates the prompt or the session was server-driven).
fn first_prompt_slug(events: &[TurnEvent]) -> String {
    let prompt = events
        .iter()
        .find_map(|e| match &e.kind {
            TurnEventKind::UserInput { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("session");
    let mut out = String::new();
    let mut prev_dash = true; // suppress leading dashes
    for ch in prompt.chars().take(40) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "session".to_string()
    } else {
        out
    }
}

/// Project the durable event stream into the export document. Pure (no I/O,
/// no session-log access) so it tests without a SessionLog: feed synthetic
/// events, assert the aggregated fields. The bridge's export() wraps this +
/// serializes + builds the filename.
/// Per-tool call counts, failures, and latency. Two passes: the first
/// establishes tool order + call counts; the second attributes fail +
/// latency via the call_id-to-tool map.
fn compute_tool_stats(events: &[TurnEvent]) -> Vec<ToolStat> {
    let mut call_to_tool: BTreeMap<&str, &str> = BTreeMap::new();
    for ev in events {
        if let TurnEventKind::ToolCall { call_id, tool, .. } = &ev.kind {
            call_to_tool.insert(call_id.as_str(), tool.as_str());
        }
    }
    let mut tool_order: Vec<&str> = Vec::new();
    let mut tool_map: BTreeMap<&str, (u64, u64, u64, u64)> = BTreeMap::new();
    for ev in events {
        if let TurnEventKind::ToolCall { tool, .. } = &ev.kind {
            let name = tool.as_str();
            if let std::collections::btree_map::Entry::Vacant(v) = tool_map.entry(name) {
                tool_order.push(name);
                v.insert((1, 0, 0, 0));
            } else {
                tool_map.get_mut(name).unwrap().0 += 1;
            }
        }
    }
    for ev in events {
        if let TurnEventKind::ToolResult {
            call_id,
            output,
            duration_ms,
        } = &ev.kind
        {
            let fail = if output.get("error").is_some() { 1 } else { 0 };
            if let Some(&tool) = call_to_tool.get(call_id.as_str()) {
                let e = tool_map.get_mut(tool).expect("tool added in pass 1");
                e.1 += fail;
                e.2 += duration_ms;
                e.3 = e.3.max(*duration_ms);
            }
        }
    }
    tool_order
        .iter()
        .map(|&name| {
            let (calls, fail, lat_total, lat_max) = tool_map[name];
            ToolStat {
                tool: name.to_string(),
                calls,
                fail,
                latency_ms_total: lat_total,
                latency_ms_max: lat_max,
            }
        })
        .collect()
}

/// Total + per-model token usage from TurnUsage events.
fn compute_usage(events: &[TurnEvent]) -> UsageSummary {
    let mut total = TokenCounts::default();
    let mut model_order: Vec<String> = Vec::new();
    let mut model_map: BTreeMap<String, TokenCounts> = BTreeMap::new();
    for ev in events {
        // One binding for the test and the field read. Testing the kind and
        // then re-matching it to pull the model out needed an unreachable arm
        // to satisfy the compiler, which is a claim about the code that the
        // reader has to verify against the line above it.
        if let TurnEventKind::TurnUsage { model, .. } = &ev.kind {
            total.add(&ev.kind);
            if !model_map.contains_key(model) {
                model_order.push(model.clone());
            }
            model_map.entry(model.clone()).or_default().add(&ev.kind);
        }
    }
    let per_model = model_order
        .iter()
        .map(|m| ModelUsage {
            model: m.clone(),
            counts: model_map.remove(m).unwrap_or_default(),
        })
        .collect();
    UsageSummary { total, per_model }
}

/// Collect compaction checkpoints and error entries from the event stream.
fn collect_checkpoints_and_errors(events: &[TurnEvent]) -> (Vec<CheckpointEntry>, Vec<ErrorEntry>) {
    let mut checkpoints: Vec<CheckpointEntry> = Vec::new();
    let mut errors: Vec<ErrorEntry> = Vec::new();
    for ev in events {
        match &ev.kind {
            TurnEventKind::CompactionBoundary { checkpoint } => checkpoints.push(CheckpointEntry {
                ts: ev.ts,
                checkpoint: Some(format!("{checkpoint:?}")),
                summary: None,
            }),
            TurnEventKind::Summary { text } => checkpoints.push(CheckpointEntry {
                ts: ev.ts,
                checkpoint: None,
                summary: Some(text.clone()),
            }),
            TurnEventKind::HookSignal {
                error: Some(_),
                reason,
                hook_name,
                tool_name,
                ..
            } => {
                errors.push(ErrorEntry {
                    ts: ev.ts,
                    kind: "hook_error".to_string(),
                    reason: reason.clone(),
                    hook_name: Some(hook_name.clone()),
                    tool_name: tool_name.clone(),
                });
            }
            TurnEventKind::TurnAborted { reason } => errors.push(ErrorEntry {
                ts: ev.ts,
                kind: "turn_aborted".to_string(),
                reason: reason.clone(),
                hook_name: None,
                tool_name: None,
            }),
            // Everything else carries no checkpoint and no error, so the
            // export skips it. Listed one by one instead of a wildcard: a new
            // event kind then fails to compile here, and whoever adds it has
            // to decide whether the export should surface it. A wildcard would
            // swallow a new error-bearing kind silently, and the export would
            // under-report for as long as nobody noticed.
            //
            // A hook signal with no error is the successful path, which is why
            // it is a skip here while the error-bearing form above is not.
            TurnEventKind::HookSignal { error: None, .. }
            | TurnEventKind::UserInput { .. }
            | TurnEventKind::TurnStarted { .. }
            | TurnEventKind::MetaUser { .. }
            | TurnEventKind::MidTurnInput { .. }
            | TurnEventKind::MemoryRecall { .. }
            | TurnEventKind::SkillListing { .. }
            | TurnEventKind::AssistantMessage { .. }
            | TurnEventKind::AssistantTextDelta { .. }
            | TurnEventKind::ToolCall { .. }
            | TurnEventKind::ToolResult { .. }
            | TurnEventKind::TurnUsage { .. }
            | TurnEventKind::RewardObservation { .. }
            | TurnEventKind::Reasoning { .. }
            | TurnEventKind::PermissionDecision { .. }
            | TurnEventKind::TruncationVerdict { .. }
            | TurnEventKind::WorktreeEnter { .. }
            | TurnEventKind::WorktreeExit { .. }
            | TurnEventKind::CacheBreak { .. }
            | TurnEventKind::SubagentSpawn { .. }
            | TurnEventKind::SubagentReturn { .. }
            | TurnEventKind::NotificationInjected { .. }
            | TurnEventKind::Unknown => {}
        }
    }
    (checkpoints, errors)
}

pub(crate) fn project_export(events: &[TurnEvent], session_id: &str, model: &str) -> ExportData {
    let started_at = events.first().map(|e| e.ts).unwrap_or(0);
    let tool_stats = compute_tool_stats(events);
    let usage = compute_usage(events);
    let (checkpoints, errors) = collect_checkpoints_and_errors(events);
    ExportData {
        session_id: session_id.to_string(),
        model: model.to_string(),
        started_at,
        trajectory: events.to_vec(),
        tool_stats,
        usage,
        checkpoints,
        errors,
    }
}

impl ExportLog for SessionLogTrajectory {
    fn export(&self) -> ExportPayload {
        let events = self.session_log.trajectory_snapshot(self.session_id);
        let data = project_export(&events, &self.session_id.to_string(), &self.model);
        let raw = serde_json::to_string_pretty(&data)
            .unwrap_or_else(|e| format!("{{\"error\": \"export serialization failed: {e}\"}}"));
        // Redact secrets on the share boundary — the export file is sent
        // outside the session (a bug report, ExPeL data, a handoff), so any
        // secret in the tool I/O / reasoning / credential fields is stripped
        // before it lands on disk. The durable log the export projects from
        // stays full-fidelity; only this shared artifact is filtered (the
        // redaction module in the tui crate).
        let json = houyicoder_tui::redaction::redact(&raw);
        let filename = format!(
            "{}-{}.json",
            format_ts_slug(data.started_at),
            first_prompt_slug(&events)
        );
        ExportPayload { filename, json }
    }
}

#[cfg(test)]
mod tests {
    //! The export projection is pure: feed synthetic events, assert the
    //! aggregated fields. Pins per-tool attribution, usage totals + per-model,
    //! fault collection, checkpoint collection, the filename slug, and the
    //! empty-log edge. The record layer's TurnUsage/ToolResult/HookSignal
    //! work is invisible in the export without them.

    use super::*;
    use houyicoder_context::{
        CheckpointId, EventId, HookErrorKind, HookEventKind, HookVerdictKind, SessionId, TurnEvent,
        TurnEventKind,
    };

    fn ev(ts: u64, kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts,
            prev_hash: None,
            kind,
        }
    }

    fn usage_event(ts: u64, model: &str, input: u64, output: u64) -> TurnEvent {
        ev(
            ts,
            TurnEventKind::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: input,
                output_tokens: output,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: model.to_string(),
                recovery: false,
                effort: None,
            },
        )
    }

    #[test]
    fn test_export_aggregates_tool_stats() {
        // Two bash ToolCalls; one result errors, one succeeds. Fail + latency
        // attribute to bash via the call_id correlation.
        let events = vec![
            ev(
                100,
                TurnEventKind::ToolCall {
                    call_id: "c1".into(),
                    tool: "bash".into(),
                    input: serde_json::json!({}),
                },
            ),
            ev(
                110,
                TurnEventKind::ToolCall {
                    call_id: "c2".into(),
                    tool: "bash".into(),
                    input: serde_json::json!({}),
                },
            ),
            ev(
                120,
                TurnEventKind::ToolResult {
                    call_id: "c1".into(),
                    output: serde_json::json!({"error": "boom"}),
                    duration_ms: 300,
                },
            ),
            ev(
                130,
                TurnEventKind::ToolResult {
                    call_id: "c2".into(),
                    output: serde_json::json!({"ok": 1}),
                    duration_ms: 120,
                },
            ),
        ];
        let data = project_export(&events, "s", "m");
        assert_eq!(data.tool_stats.len(), 1, "one tool (bash)");
        let bash = &data.tool_stats[0];
        assert_eq!(bash.tool, "bash");
        assert_eq!(bash.calls, 2);
        assert_eq!(bash.fail, 1);
        assert_eq!(bash.latency_ms_total, 420);
        assert_eq!(bash.latency_ms_max, 300);
    }

    #[test]
    fn test_export_usage_breakdown() {
        let events = vec![
            usage_event(100, "haiku", 1000, 200),
            usage_event(110, "haiku", 500, 100),
            usage_event(120, "sonnet", 3000, 400),
        ];
        let data = project_export(&events, "s", "m");
        assert_eq!(data.usage.total.input_tokens, 4500);
        assert_eq!(data.usage.total.output_tokens, 700);
        // per-model in first-seen order: haiku then sonnet.
        assert_eq!(data.usage.per_model.len(), 2);
        assert_eq!(data.usage.per_model[0].model, "haiku");
        assert_eq!(data.usage.per_model[0].counts.input_tokens, 1500);
        assert_eq!(data.usage.per_model[1].model, "sonnet");
        assert_eq!(data.usage.per_model[1].counts.input_tokens, 3000);
    }

    #[test]
    fn test_export_collects_faults() {
        // A hook fault (error = Some) lands in errors; a bare-Allow
        // HookSignal does NOT (the record layer skips Allow, and the export
        // only collects error-bearing signals). TurnAborted also lands.
        let hook_fault = ev(
            100,
            TurnEventKind::HookSignal {
                event: HookEventKind::default(),
                verdict: HookVerdictKind::Deny,
                error: Some(HookErrorKind::Timeout),
                reason: "timed out".into(),
                hook_name: "guard".into(),
                tool_name: Some("bash".into()),
                triggered_event: None,
                turn: Some(1),
                call_in_turn: Some(1),
            },
        );
        let aborted = ev(
            110,
            TurnEventKind::TurnAborted {
                reason: "crashed".into(),
            },
        );
        let data = project_export(&[hook_fault, aborted], "s", "m");
        assert_eq!(data.errors.len(), 2);
        assert_eq!(data.errors[0].kind, "hook_error");
        assert_eq!(data.errors[0].hook_name.as_deref(), Some("guard"));
        assert_eq!(data.errors[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(data.errors[1].kind, "turn_aborted");
        assert_eq!(data.errors[1].reason, "crashed");
    }

    #[test]
    fn test_export_collects_checkpoints() {
        let events = vec![
            ev(
                100,
                TurnEventKind::CompactionBoundary {
                    checkpoint: CheckpointId::new(),
                },
            ),
            ev(
                110,
                TurnEventKind::Summary {
                    text: "folded T1-T3".into(),
                },
            ),
        ];
        let data = project_export(&events, "s", "m");
        assert_eq!(data.checkpoints.len(), 2);
        assert!(data.checkpoints[0].checkpoint.is_some());
        assert_eq!(data.checkpoints[1].summary.as_deref(), Some("folded T1-T3"));
    }

    #[test]
    fn test_export_filename_slug() {
        // 2026-08-03 20:25 UTC = epoch ms 1785788700000. Verify the slug
        // formatter produces the expected stamp + the prompt slugifier trims
        // punctuation. format_ts_slug is UTC + deterministic.
        assert_eq!(format_ts_slug(1_785_788_700_000), "2026-08-03-2025");
        let events = vec![ev(
            100,
            TurnEventKind::UserInput {
                text: "Fix the bug!! (urgent)".into(),
            },
        )];
        assert_eq!(first_prompt_slug(&events), "fix-the-bug-urgent");
    }

    #[test]
    fn test_export_empty_is_empty() {
        let data = project_export(&[], "s", "m");
        assert_eq!(data.started_at, 0);
        assert!(data.tool_stats.is_empty());
        assert_eq!(data.usage.total.input_tokens, 0);
        assert!(data.errors.is_empty());
        assert_eq!(
            first_prompt_slug(&[]),
            "session",
            "no prompt => slug session"
        );
        assert_eq!(
            format_ts_slug(0),
            "1970-01-01-0000",
            "epoch zero => 1970 slug"
        );
    }

    #[test]
    fn test_export_redacts_secrets_trajectory() {
        // A tool result whose output carries an OpenAI key. The export
        // serializes the full trajectory then redacts on the share boundary,
        // so the shared JSON file must NOT carry the secret — even though
        // the durable log the export projects from stays full-fidelity.
        let secret = "sk-abcd1234efgh5678ijkl9012mnop3456qrst";
        let events = vec![ev(
            100,
            TurnEventKind::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"token": secret}),
                duration_ms: 0,
            },
        )];
        let data = project_export(&events, "s", "m");
        let raw = serde_json::to_string(&data).unwrap();
        let redacted = houyicoder_tui::redaction::redact(&raw);
        assert!(
            redacted.contains("[REDACTED"),
            "the secret must be redacted in the export json, got: {redacted}"
        );
        assert!(
            !redacted.contains(secret),
            "the raw secret must not land in the export file, got: {redacted}"
        );
    }
}
