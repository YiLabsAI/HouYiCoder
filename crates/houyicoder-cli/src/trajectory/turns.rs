//! Turn assembly: grouping the durable event stream into user turns and their
//! records.
//!
//! A turn is one user input. This module owns the accumulator that folds every
//! event of a turn — model calls, tool calls, delegations, context — into the
//! records and totals the trajectory view renders.

use std::collections::{HashMap, HashSet};

use super::view::{CallIndex, fmt_bytes, preview, result_failed};
use houyicoder_context::{HookVerdictKind, SessionEvent, SessionLogEntry};
use houyicoder_tui::result_body::extract_body;
use houyicoder_tui::view::trajectory_pane::{
    CompactedBoundary, EventTiming, EventUsage, ModelSwitchBoundary, RecordOutcome,
    TrajectoryRecord, TrajectoryRecordKind, TrajectoryTurn, TurnBoundary,
};

/// Accumulates one user turn: its records, its summed totals, and the
/// open model call, tool call, and delegation its later events attach to.
/// Reset at each turn boundary; flushed into the turn list when the next
/// user input arrives.
pub(super) struct TurnBuilder {
    records: Vec<TrajectoryRecord>,
    boundary_before: Vec<TurnBoundary>,
    user_input: String,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning_tokens: Option<u64>,
    models: Vec<String>,
    efforts: Vec<String>,
    tool_count: usize,
    /// Model calls made in this turn, so each is numbered within it.
    model_calls: u32,
    tool_fail: usize,
    retries: usize,
    first_ts: Option<u64>,
    last_ts: u64,
    run_completed_ms: u64,
    success: bool,
    /// Index of the model record currently open, so its thinking, reply, usage,
    /// and timing attach to one row instead of scattering across several.
    open_model: Option<usize>,
    /// Tool calls awaiting their result, keyed by call id.
    open_tools: HashMap<String, usize>,
    /// Delegations awaiting their return, keyed by child session id.
    open_agents: HashMap<String, usize>,
}

impl TurnBuilder {
    pub(super) fn new() -> Self {
        Self {
            records: Vec::new(),
            boundary_before: Vec::new(),
            user_input: String::new(),
            tokens_in: None,
            tokens_out: None,
            cache_read: None,
            cache_write: None,
            reasoning_tokens: None,
            models: Vec::new(),
            efforts: Vec::new(),
            tool_count: 0,
            model_calls: 0,
            tool_fail: 0,
            retries: 0,
            first_ts: None,
            last_ts: 0,
            run_completed_ms: 0,
            success: true,
            open_model: None,
            open_tools: HashMap::new(),
            open_agents: HashMap::new(),
        }
    }

    fn reset(&mut self, user_input: String, ts: u64, boundaries: Vec<TurnBoundary>) {
        self.records.clear();
        self.boundary_before = boundaries;
        self.user_input = user_input;
        self.tokens_in = None;
        self.tokens_out = None;
        self.cache_read = None;
        self.cache_write = None;
        self.reasoning_tokens = None;
        self.models.clear();
        self.efforts.clear();
        self.tool_count = 0;
        self.model_calls = 0;
        self.tool_fail = 0;
        self.retries = 0;
        self.first_ts = Some(ts);
        self.last_ts = ts;
        self.run_completed_ms = 0;
        self.success = true;
        self.open_model = None;
        self.open_tools.clear();
        self.open_agents.clear();
    }

    pub(super) fn is_open(&self) -> bool {
        self.first_ts.is_some()
    }

    /// True once this turn has reported usage. The first usage of a turn is
    /// where a switch between turns becomes visible, since the model id only
    /// appears in the usage event.
    fn has_usage(&self) -> bool {
        self.tokens_in.is_some()
    }

    /// Offset of a durable timestamp from this turn's first event.
    fn offset(&self, ts: u64) -> u64 {
        ts.saturating_sub(self.first_ts.unwrap_or(ts))
    }

    fn touch(&mut self, ts: u64) {
        if self.first_ts.is_none() {
            self.first_ts = Some(ts);
        }
        self.last_ts = self.last_ts.max(ts);
    }

    /// Append a record and return its index.
    fn push(&mut self, record: TrajectoryRecord) -> usize {
        self.records.push(record);
        self.records.len() - 1
    }

    fn record(
        &self,
        kind: TrajectoryRecordKind,
        name: Option<String>,
        summary: String,
        ts: u64,
    ) -> TrajectoryRecord {
        // Context, memory, and compaction rows are facts that already
        // happened; an error row is a failure marker. A tool, agent, or model
        // call is only pending until its completion lands.
        let outcome = match kind {
            TrajectoryRecordKind::Error => RecordOutcome::Failed,
            TrajectoryRecordKind::Tool
            | TrajectoryRecordKind::Agent
            | TrajectoryRecordKind::Model => RecordOutcome::Pending,
            TrajectoryRecordKind::Context
            | TrajectoryRecordKind::Memory
            | TrajectoryRecordKind::Hook
            | TrajectoryRecordKind::Compaction => RecordOutcome::Ok,
        };
        TrajectoryRecord {
            kind,
            name,
            ordinal: 0,
            summary,
            start_ms: self.offset(ts),
            duration_ms: 0,
            outcome,
            thinking: None,
            input: None,
            output: None,
            usage: None,
            timing: None,
            retries: 0,
        }
    }

    /// Open a model call. A call already open (a provider retry inside the same
    /// round trip) is reused rather than duplicated.
    fn open_model_call(&mut self, model: Option<String>, ts: u64) {
        self.touch(ts);
        self.model_calls += 1;
        let ordinal = self.model_calls;
        let name = model.clone();
        let index = self.push(self.record(TrajectoryRecordKind::Model, name, String::new(), ts));
        self.records[index].ordinal = ordinal;
        self.open_model = Some(index);
    }

    fn current_model_index(&mut self, ts: u64) -> usize {
        match self.open_model {
            Some(i) => i,
            None => {
                self.open_model_call(None, ts);
                self.open_model.unwrap_or(0)
            }
        }
    }

    /// Fold the assistant's reply and thinking into the open model call.
    fn attach_assistant(&mut self, text: &str, thinking: &Option<String>, ts: u64) {
        self.touch(ts);
        let index = self.current_model_index(ts);
        let record = &mut self.records[index];
        if !text.is_empty() {
            record.summary = preview(text);
            record.output = Some(text.to_string());
            // A reply means the call completed; without this a call that never
            // recorded timing would stay pending forever.
            record.outcome = RecordOutcome::Ok;
        }
        if let Some(t) = thinking
            && !t.is_empty()
        {
            record.thinking = Some(t.clone());
            if record.summary.is_empty() {
                record.summary = preview(t);
            }
        }
    }

    /// Fold a streamed reasoning chunk into the open model call.
    fn attach_reasoning(&mut self, text: &str, ts: u64) {
        self.touch(ts);
        let index = self.current_model_index(ts);
        let record = &mut self.records[index];
        match &mut record.thinking {
            Some(existing) => existing.push_str(text),
            None => record.thinking = Some(text.to_string()),
        }
        if record.summary.is_empty() {
            record.summary = preview(text);
        }
    }

    fn attach_timing(
        &mut self,
        total_ms: u64,
        ttft_ms: Option<u64>,
        decode_ms: Option<u64>,
        ts: u64,
    ) {
        self.touch(ts);
        let index = self.current_model_index(ts);
        self.records[index].outcome = RecordOutcome::Ok;
        self.records[index].timing = Some(EventTiming {
            total_ms,
            ttft_ms,
            decode_ms,
        });
        // A model call's own span is the honest bar width for that row.
        if self.records[index].duration_ms == 0 {
            self.records[index].duration_ms = total_ms;
        }
    }

    /// Sum one model call's usage into the turn and onto the call's own record.
    fn apply_usage(&mut self, ev: &SessionEvent, ts: u64) {
        let SessionEvent::TurnUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            model: ev_model,
            effort,
            recovery,
            ..
        } = ev
        else {
            return;
        };
        self.touch(ts);
        self.tokens_in = Some(self.tokens_in.unwrap_or(0) + *input_tokens);
        self.tokens_out = Some(self.tokens_out.unwrap_or(0) + *output_tokens);
        self.cache_read = Some(self.cache_read.unwrap_or(0) + *cache_read_input_tokens);
        self.cache_write = Some(self.cache_write.unwrap_or(0) + *cache_write_input_tokens);
        self.reasoning_tokens = Some(self.reasoning_tokens.unwrap_or(0) + *reasoning_tokens);
        if !ev_model.is_empty() && !self.models.contains(ev_model) {
            self.models.push(ev_model.clone());
        }
        if let Some(e) = effort
            && !self.efforts.contains(e)
        {
            self.efforts.push(e.clone());
        }
        if *recovery {
            self.retries += 1;
        }

        let index = self.current_model_index(ts);
        let record = &mut self.records[index];
        if record.name.is_none() && !ev_model.is_empty() {
            record.name = Some(ev_model.clone());
        }
        // A provider retry reuses this record, so its usage accumulates the
        // same way the turn's does; overwriting would drop the retry's cost
        // from the call that caused it.
        let prior = record.usage.unwrap_or_default();
        record.usage = Some(EventUsage {
            input: Some(prior.input.unwrap_or(0) + *input_tokens),
            output: Some(prior.output.unwrap_or(0) + *output_tokens),
            cache_read: Some(prior.cache_read.unwrap_or(0) + *cache_read_input_tokens),
            cache_write: Some(prior.cache_write.unwrap_or(0) + *cache_write_input_tokens),
            reasoning: Some(prior.reasoning.unwrap_or(0) + *reasoning_tokens),
        });
        if *recovery {
            record.retries += 1;
        }
    }

    /// Open a tool record. The matching result completes this same record, so
    /// one tool call is one row from call to result.
    fn begin_tool(&mut self, call_id: &str, tool: &str, input: &serde_json::Value, ts: u64) {
        self.touch(ts);
        self.tool_count += 1;
        let ordinal = self.tool_count;
        let index = self.push(self.record(
            TrajectoryRecordKind::Tool,
            Some(tool.to_string()),
            format!("{tool}({})", preview(&input.to_string())),
            ts,
        ));
        self.records[index].ordinal = ordinal as u32;
        self.records[index].input = Some(input.to_string());
        self.open_tools.insert(call_id.to_string(), index);
    }

    fn finish_tool(
        &mut self,
        call_id: &str,
        output: &serde_json::Value,
        duration_ms: u64,
        failed: bool,
        ts: u64,
    ) {
        self.touch(ts);
        // Format the tool output the same way the transcript does — a failed
        // command shows its exit code and stderr, an edit shows its diff
        // summary. One rendering path for tool results, not two that drift.
        let body = extract_body(&output.to_string());
        let index = match self.open_tools.remove(call_id) {
            Some(index) => index,
            // A result whose call sits outside the loaded window: show the
            // result on its own row rather than dropping it silently.
            None => {
                self.tool_count += 1;
                self.push(self.record(TrajectoryRecordKind::Tool, None, preview(&body), ts))
            }
        };
        let record = &mut self.records[index];
        record.summary = preview(&body);
        record.output = Some(body);
        record.duration_ms = duration_ms;
        record.outcome = if failed {
            RecordOutcome::Failed
        } else {
            RecordOutcome::Ok
        };
        if failed {
            self.tool_fail += 1;
        }
    }

    /// Open a delegation record, completed by the matching return.
    fn begin_agent(
        &mut self,
        child_session_id: &str,
        subagent_type: &str,
        prompt_summary: &str,
        ts: u64,
    ) {
        self.touch(ts);
        let summary = if prompt_summary.is_empty() {
            format!("{subagent_type} delegated")
        } else {
            preview(prompt_summary)
        };
        let index = self.push(self.record(
            TrajectoryRecordKind::Agent,
            Some(subagent_type.to_string()),
            summary,
            ts,
        ));
        self.open_agents.insert(child_session_id.to_string(), index);
    }

    fn finish_agent(
        &mut self,
        child_session_id: &str,
        status: &str,
        summary: &str,
        usage: Option<EventUsage>,
        ts: u64,
    ) {
        self.touch(ts);
        let index = match self.open_agents.remove(child_session_id) {
            Some(index) => index,
            // The spawn sits outside the loaded window: show the return on its
            // own record rather than dropping it silently.
            None => {
                let record = self.record(
                    TrajectoryRecordKind::Agent,
                    None,
                    format!("{child_session_id} returned"),
                    ts,
                );
                self.push(record)
            }
        };
        let end_ms = self.offset(ts);
        let record = &mut self.records[index];
        if record.start_ms < end_ms {
            record.duration_ms = end_ms - record.start_ms;
        }
        // An unrecognised status is unknown, not a success.
        record.outcome = match status {
            "completed" | "ok" => RecordOutcome::Ok,
            "" => RecordOutcome::Pending,
            _ => RecordOutcome::Failed,
        };
        // A child's usage also accumulates into the turn's own tokens, so the
        // turn row reports what this user request spent overall.
        if let Some(child_u) = usage {
            if let Some(tin) = child_u.input {
                self.tokens_in = Some(self.tokens_in.unwrap_or(0) + tin);
            }
            if let Some(tout) = child_u.output {
                self.tokens_out = Some(self.tokens_out.unwrap_or(0) + tout);
            }
            if let Some(cread) = child_u.cache_read {
                self.cache_read = Some(self.cache_read.unwrap_or(0) + cread);
            }
            if let Some(cwrite) = child_u.cache_write {
                self.cache_write = Some(self.cache_write.unwrap_or(0) + cwrite);
            }
        }
        record.usage = usage;
        if !summary.is_empty() {
            record.output = Some(summary.to_string());
        }
    }

    /// Append a record for a turn-level signal: an abort, a compaction, or a
    /// hook verdict. These carry no completion of their own.
    fn push_signal(
        &mut self,
        kind: TrajectoryRecordKind,
        name: Option<String>,
        text: &str,
        ts: u64,
    ) {
        self.touch(ts);
        let record = self.record(kind, name, preview(text), ts);
        self.push(record);
    }

    pub(super) fn flush(&mut self, turns: &mut Vec<TrajectoryTurn>, n: usize) {
        // A delegation with no return in this window stays open: keep it
        // visible with no duration rather than inventing an end.
        let wall_ms = self
            .last_ts
            .saturating_sub(self.first_ts.unwrap_or(self.last_ts));
        turns.push(TrajectoryTurn {
            n,
            boundary_before: std::mem::take(&mut self.boundary_before),
            user_input: std::mem::take(&mut self.user_input),
            tokens_in: self.tokens_in.map(|v| v as usize),
            tokens_out: self.tokens_out.map(|v| v as usize),
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            models: std::mem::take(&mut self.models),
            efforts: std::mem::take(&mut self.efforts),
            reasoning_tokens: self.reasoning_tokens.map(|v| v as usize),
            tool_count: self.tool_count,
            tool_fail: self.tool_fail,
            retries: self.retries,
            // The turn's wall time is its own event span, which is what the
            // user waited. RunCompleted is a coarser second-resolution figure
            // from the run leg, used only when it reaches further.
            duration_ms: wall_ms.max(self.run_completed_ms),
            success: self.success,
            records: std::mem::take(&mut self.records),
        });
        self.first_ts = None;
    }
}

/// Open or close turns. Returns true when the event was a turn boundary, so
/// the caller does not also fold it in as turn content.
///
/// A user input opens a turn. The model-call boundary that follows stays inside
/// it, so a prompt that needs several round trips is still one turn.
pub(super) fn apply_turn_boundary(
    builder: &mut TurnBuilder,
    ev: &SessionLogEntry,
    turns: &mut Vec<TrajectoryTurn>,
    n: &mut usize,
    pending_boundary: &mut Vec<TurnBoundary>,
    last_model: &mut Option<String>,
) -> bool {
    match &ev.event {
        SessionEvent::ContextCleared { prior_turn } => {
            pending_boundary.push(TurnBoundary::ContextCleared {
                prior_turn: *prior_turn,
                at_secs: ev.ts / 1000,
            });
        }
        SessionEvent::CompactionBoundary {
            checkpoint,
            pre_tokens,
            post_tokens,
        } => {
            pending_boundary.push(TurnBoundary::Compacted(Box::new(CompactedBoundary {
                checkpoint_id: checkpoint.to_string(),
                pre_tokens: *pre_tokens,
                post_tokens: *post_tokens,
                at_secs: ev.ts / 1000,
            })));
        }
        SessionEvent::TurnUsage { model, .. } => {
            if !model.is_empty() {
                if let Some(prev) = last_model.as_ref()
                    && prev != model
                {
                    let boundary = TurnBoundary::ModelSwitch(Box::new(ModelSwitchBoundary {
                        from: prev.clone(),
                        to: model.clone(),
                        at_secs: ev.ts / 1000,
                    }));
                    if !builder.is_open() {
                        pending_boundary.push(boundary);
                    } else if !builder.has_usage() {
                        // The turn's first usage: the switch happened between
                        // turns, so it belongs above this one. A later usage is
                        // a switch inside the turn, which its own model records
                        // already show.
                        builder.boundary_before.push(boundary);
                    }
                }
                *last_model = Some(model.clone());
            }
            return false;
        }
        SessionEvent::UserInput { text } => {
            if builder.is_open() {
                builder.flush(turns, *n);
            }
            *n += 1;
            builder.reset(text.clone(), ev.ts, std::mem::take(pending_boundary));
            let record = builder.record(TrajectoryRecordKind::Context, None, preview(text), ev.ts);
            let index = builder.push(record);
            builder.records[index].input = Some(text.clone());
        }
        SessionEvent::MidTurnInput { text, .. } => {
            builder.touch(ev.ts);
            let summary = format!("User update: {}", preview(text));
            let record = builder.record(
                TrajectoryRecordKind::Context,
                Some("User update".into()),
                summary,
                ev.ts,
            );
            let index = builder.push(record);
            builder.records[index].input = Some(text.clone());
        }
        SessionEvent::TurnStarted { .. } => {
            // A log that opens mid-run has model calls before any user input;
            // open a turn so those records have a home.
            if !builder.is_open() {
                *n += 1;
                builder.reset(String::new(), ev.ts, std::mem::take(pending_boundary));
            }
            builder.open_model_call(None, ev.ts);
        }
        _ => return false,
    }
    true
}

/// Fold an event that belongs to the turn already open.
pub(super) fn apply_turn_content(
    builder: &mut TurnBuilder,
    ev: &SessionLogEntry,
    calls: &CallIndex,
    spawned: &HashSet<&str>,
) {
    match &ev.event {
        SessionEvent::ModelStepTiming {
            total_ms,
            ttft_ms,
            decode_ms,
            ..
        } => builder.attach_timing(*total_ms, *ttft_ms, *decode_ms, ev.ts),
        SessionEvent::TurnUsage { .. } => builder.apply_usage(&ev.event, ev.ts),
        SessionEvent::AssistantMessage { text, thinking } => {
            builder.attach_assistant(text, thinking, ev.ts);
        }
        SessionEvent::Reasoning { text } => builder.attach_reasoning(text, ev.ts),
        // A delegation's own tool call is represented by its Agent record;
        // showing the tool as well would count the same work twice.
        SessionEvent::ToolCall {
            call_id,
            tool,
            input,
        } => {
            if !spawned.contains(call_id.as_str()) {
                builder.begin_tool(call_id, tool, input, ev.ts);
            }
        }
        SessionEvent::ToolResult {
            call_id,
            output,
            duration_ms,
        } => {
            if spawned.contains(call_id.as_str()) {
                return;
            }
            let failed = result_failed(output, call_id, calls);
            builder.finish_tool(call_id, output, *duration_ms, failed, ev.ts);
        }
        SessionEvent::SubagentSpawn {
            child_session_id,
            subagent_type,
            prompt_summary,
            ..
        } => builder.begin_agent(child_session_id, subagent_type, prompt_summary, ev.ts),
        SessionEvent::SubagentReturn {
            child_session_id,
            status,
            summary,
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            ..
        } => {
            let usage = (*input_tokens > 0 || *output_tokens > 0).then_some(EventUsage {
                input: Some(*input_tokens),
                output: Some(*output_tokens),
                cache_read: Some(*cache_read_input_tokens),
                cache_write: Some(*cache_write_input_tokens),
                reasoning: Some(*reasoning_tokens),
            });
            builder.finish_agent(child_session_id, status, summary, usage, ev.ts);
        }
        SessionEvent::MemoryRecall { keys, bytes, .. } => {
            builder.touch(ev.ts);
            // The row names the recall and its count; the size and the keys
            // themselves belong to the drill-down, where there is room for
            // them.
            let summary = if keys.is_empty() {
                "Recall".to_string()
            } else {
                format!("Recall {} items", keys.len())
            };
            let record = builder.record(TrajectoryRecordKind::Memory, None, summary, ev.ts);
            let index = builder.push(record);
            // A log written before the size was recorded carries zero, which
            // is an absent measurement rather than an empty recall.
            if *bytes > 0 {
                builder.records[index].input = Some(format!("{} injected", fmt_bytes(*bytes)));
            }
            builder.records[index].output = Some(keys.join("\n"));
        }
        SessionEvent::HookSignal {
            verdict,
            reason,
            hook_name,
            ..
        } => apply_hook_signal(builder, verdict, reason, hook_name, ev.ts),
        SessionEvent::TurnAborted { reason } => {
            builder.success = false;
            builder.push_signal(TrajectoryRecordKind::Error, None, reason, ev.ts);
        }
        SessionEvent::Summary { text } => {
            builder.push_signal(TrajectoryRecordKind::Compaction, None, text, ev.ts);
        }
        SessionEvent::CompactionBoundary { checkpoint, .. } => {
            builder.push_signal(
                TrajectoryRecordKind::Compaction,
                None,
                &format!("checkpoint {checkpoint}"),
                ev.ts,
            );
        }
        SessionEvent::RunCompleted { secs: Some(secs) } => {
            builder.run_completed_ms = (*secs as u64) * 1000;
        }
        _ => {}
    }
}

/// Fold a hook verdict into the turn.
///
/// A bare Allow is derivable from absence and carries no signal. A Deny is a
/// failure and is drawn as one. Every other verdict is a control-flow signal
/// that did not fail — an observation, an injection, a request for the user —
/// so it becomes a neutral hook record rather than a red error.
fn apply_hook_signal(
    builder: &mut TurnBuilder,
    verdict: &HookVerdictKind,
    reason: &str,
    hook_name: &str,
    ts: u64,
) {
    let kind = match verdict {
        HookVerdictKind::Allow => return,
        HookVerdictKind::Deny => TrajectoryRecordKind::Error,
        _ => TrajectoryRecordKind::Hook,
    };
    let name = if hook_name.is_empty() {
        "hook".to_string()
    } else {
        hook_name.to_string()
    };
    builder.touch(ts);
    let record = builder.record(
        kind,
        Some(name),
        format!("{verdict:?} {}", preview(reason)),
        ts,
    );
    builder.push(record);
}
