//! Engine-to-wire boundary adapter. Pure mapping from engine types to
//! protocol wire types — no I/O, no state — so the server loop stays apart
//! and under the file-size gate.

pub(crate) mod compaction;
pub(crate) mod memory_view;
pub(crate) mod redundancy;
pub(crate) mod session_descriptor;
mod trajectory_row;
use trajectory_row::{event_name, hex_short};

use houyicoder_context::SessionEvent;
use houyicoder_protocol::acp_wire::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::frontend::run::{
    ApprovalDecision, ApprovalRequest, ContentBlock, RunError, RunOutcome, RunResult, StopReason,
};
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use houyicoder_protocol::frontend::status::StatusSnapshot as WireStatusSnapshot;
pub(crate) use session_descriptor::map_session_descriptor;

/// Map the engine run result to the protocol message form. Outcome
/// variants match the engine enum one-for-one except Interruption (a
/// mid-turn permission ask is a reverse request, not an outcome) and
/// VerifyFailure (collapses to a summary string).
pub(crate) fn map_run_result(run: &houyicoder_core::agent::RunResult) -> RunResult {
    let (outcome, stop_reason) = match &run.outcome {
        houyicoder_core::agent::RunOutcome::FinalOutput(text) => (
            RunOutcome::FinalOutput {
                content: vec![ContentBlock::Text { text: text.clone() }],
            },
            StopReason::EndTurn,
        ),
        houyicoder_core::agent::RunOutcome::Handoff(agent) => (
            RunOutcome::Handoff {
                agent: agent.0.clone(),
            },
            // The turn ended; the handoff detail travels the acpx side channel
            // when that lands. The stop reason is the legal end-turn value.
            StopReason::EndTurn,
        ),
        houyicoder_core::agent::RunOutcome::Interrupted(reason) => (
            RunOutcome::Interrupted {
                reason: reason.clone(),
            },
            StopReason::Cancelled,
        ),
        houyicoder_core::agent::RunOutcome::VerifyFailed(failure) => {
            let summary = if failure.checks.is_empty() {
                "verify failed".to_string()
            } else {
                failure.checks.join("; ")
            };
            (
                RunOutcome::VerifyFailed { summary },
                // Verify-failed is a turn-end; the rich detail rides the
                // acpx side channel. The stop reason is the legal end-turn.
                StopReason::EndTurn,
            )
        }
        houyicoder_core::agent::RunOutcome::MaxTurnsReached { turns } => (
            RunOutcome::MaxTurnsReached { turns: *turns },
            // Graceful max-turns ceiling; the run is resumable.
            StopReason::MaxTurnRequests,
        ),
        // Interruption never reaches the wire: the turn loop drives the
        // reverse-request + resume loop and only calls map_run_result on
        // a final outcome. Reaching this arm is a logic bug; fail visibly.
        houyicoder_core::agent::RunOutcome::Interruption(_) => {
            unreachable!("interruption is resolved by the reverse-request loop, not mapped to wire")
        }
    };
    RunResult {
        outcome,
        turns: run.turns,
        usage: run.usage.clone(),
        stop_reason,
    }
}

/// Build the wire form of an approval request the engine surfaced. The input
/// the model passed travels verbatim so the frontend can render it or inject
/// an answer-populated updated_input on resume. The reason is the structured
/// Ask the gate produced; None only when the composition root could not
/// reconstruct one, in which case the card renders a generic prompt.
pub(crate) fn build_approval_request(
    req: &houyicoder_core::agent::ApprovalRequest,
    reason: Option<&houyicoder_permission::AskReason>,
    delegation: Option<&houyicoder_protocol::frontend::run::DelegationSource>,
) -> ApprovalRequest {
    ApprovalRequest {
        call_id: req.call_id.clone(),
        tool_name: req.tool_name.clone(),
        input: req.input.clone(),
        options: Vec::new(),
        reason: reason.map(houyicoder_protocol::frontend::permission::AskReason::from),
        delegation: delegation.cloned(),
    }
}

/// Parse a caller's approval decision from the wire form into the engine
/// type. updated_input travels verbatim so an answer-populated input the
/// human-in-the-loop UI injected reaches the tool on resume. The wire scope
/// field is consumed at the boundary (the server records it on the
/// PermissionDecision audit event) and does not cross into the engine.
pub(crate) fn parse_approval_decision(
    d: ApprovalDecision,
) -> houyicoder_core::agent::ApprovalDecision {
    houyicoder_core::agent::ApprovalDecision {
        call_id: d.call_id,
        approved: d.approved,
        updated_input: d.updated_input,
    }
}

/// Map the engine runner status snapshot to the wire form. The engine
/// snapshot carries a borrowed breaker-state label and a Duration cool-down;
/// the wire form owns both so the TUI renders /status without importing the
/// engine or resilience crate.
pub(crate) fn map_status_snapshot(
    s: &houyicoder_core::agent::StatusSnapshot,
) -> WireStatusSnapshot {
    WireStatusSnapshot {
        model: s.model.clone(),
        breaker_state: s.breaker_state.map(String::from),
        breaker_reason: s.breaker_reason.clone(),
        breaker_cool_down_secs: s.breaker_cool_down.map(|d| d.as_secs()),
        cumulative_usage: s.cumulative_usage.clone(),
        last_input_tokens: s.last_input_tokens,
        context_window: s.context_window,
        tool_calls: s.tool_calls,
        tool_success: s.tool_success,
        tool_errors: s.tool_errors,
        // Sidecar + env-config (descriptor, auth, base_url, setting_sources)
        // attach server-side; the engine snapshot has none. ..Default fills
        // them.
        ..Default::default()
    }
}

/// Build the wire audit-log form of the trajectory. One row per event across
/// all kinds (including those with no session/update counterpart), each
/// carrying its event id and prev_hash so the TUI can verify the chain is
/// intact.
pub(crate) fn build_trajectory_entries(
    events: &[houyicoder_context::SessionLogEntry],
) -> Vec<houyicoder_protocol::frontend::trajectory::TrajectoryEntry> {
    use houyicoder_protocol::frontend::trajectory::TrajectoryEntry;
    events
        .iter()
        .map(|ev| {
            let duration_ms = match &ev.event {
                houyicoder_context::SessionEvent::ToolResult { duration_ms, .. } => {
                    Some(*duration_ms)
                }
                _ => None,
            };
            TrajectoryEntry {
                kind: event_name(&ev.event).to_string(),
                ts: ev.ts,
                event_id: ev.id.to_string(),
                prev_hash: ev.prev_hash.as_ref().map(|h| hex_short(&h.0)),
                duration_ms,
            }
        })
        .collect()
}

/// Map the engine context-window breakdown to the wire form so the TUI
/// renders /context without importing the engine or context crate.
pub(crate) fn map_context_breakdown(
    bd: &houyicoder_core::agent::ContextBreakdown,
) -> houyicoder_protocol::frontend::context::ContextBreakdown {
    use houyicoder_protocol::frontend::context::{
        CategoryBreakdown as WireCat, ContextBreakdown as WireBd, GridSquare as WireGrid,
    };
    let grid: Vec<Vec<WireGrid>> = bd
        .grid
        .iter()
        .map(|row| {
            row.iter()
                .map(|sq| WireGrid {
                    category_idx: sq.category_idx,
                    fullness: sq.fullness,
                })
                .collect()
        })
        .collect();
    // Compute the cache breakpoint as a flat grid cell index: the cell where
    // the cached prefix (system prompt + tools) ends. Cells [0, bp) are the
    // cached prefix; bp onward is the per-turn fresh suffix. Derived from
    // cache_prefix_tokens / context_window scaled to the grid cell count, so
    // it stays in sync with the grid the adapter just built (not a stale
    // engine-side value). None when the prefix or window is unknown or the
    // grid is empty.
    let total_cells: usize = grid.iter().map(|r| r.len()).sum();
    let cache_breakpoint = match (bd.cache_prefix_tokens, bd.context_window) {
        (Some(prefix), window) if window > 0 && total_cells > 0 => {
            let bp = (prefix as f64 / window as f64 * total_cells as f64).round() as usize;
            Some(bp.min(total_cells.saturating_sub(1)))
        }
        _ => None,
    };
    WireBd {
        model: bd.model.clone(),
        total_tokens: bd.total_tokens,
        context_window: bd.context_window,
        categories: bd
            .categories
            .iter()
            .map(|c| WireCat {
                label: c.label.clone(),
                color_hint: c.color_hint,
                tokens: c.tokens,
                is_deferred: c.is_deferred,
                is_reserved: c.is_reserved,
            })
            .collect(),
        grid,
        cache_breakpoint,
        compact_summary: bd.compact_summary.clone(),
        cache_prefix_tokens: bd.cache_prefix_tokens,
        cache_hit_rate: bd.cache_hit_rate,
    }
}

/// Map a run failure to the protocol message form. The kind is the
/// variant name the frontend records; the message is the Display string it
/// surfaces as an error line.
pub(crate) fn map_run_error(e: &houyicoder_core::agent::RunError) -> RunError {
    let kind = match e {
        houyicoder_core::agent::RunError::Context(..) => "context",
        houyicoder_core::agent::RunError::ProviderFatal(..) => "provider_fatal",
        houyicoder_core::agent::RunError::ProviderExhausted(..) => "provider_exhausted",
        houyicoder_core::agent::RunError::MaxTurnsExceeded { .. } => "fork_max_turns_exceeded",
        houyicoder_core::agent::RunError::ContextOverflowBounded { .. } => {
            "context_overflow_bounded"
        }
        houyicoder_core::agent::RunError::ContextOverflowNoProgress => {
            "context_overflow_no_progress"
        }
    };
    // The message branches on the inner ProviderError so the user sees an
    // actionable hint, not a generic "provider fatal". Auth points at the
    // API key; ModelNotFound points at the catalog and never mentions the key
    // (the design's "don't mislead" rule — a model typo read as a key error
    // sends the user debugging credentials, not the model id).
    let message = match e {
        houyicoder_core::agent::RunError::ProviderFatal(
            houyicoder_protocol::llm::ProviderError::Auth,
        )
        | houyicoder_core::agent::RunError::ProviderExhausted(
            houyicoder_protocol::llm::ProviderError::Auth,
        ) => "authentication failed — check your API key (DASHSCOPE_API_KEY or OPENAI_API_KEY)"
            .to_string(),
        houyicoder_core::agent::RunError::ProviderFatal(
            houyicoder_protocol::llm::ProviderError::ModelNotFound(m),
        )
        | houyicoder_core::agent::RunError::ProviderExhausted(
            houyicoder_protocol::llm::ProviderError::ModelNotFound(m),
        ) => format!(
            "model not found ({m}) — check model.catalog in settings.json, the id may be misspelled"
        ),
        other => other.to_string(),
    };
    RunError {
        kind: kind.to_string(),
        message,
    }
}

/// Map the engine permission mode to the wire form.
pub(crate) fn permission_mode_to_wire(
    mode: houyicoder_permission::PermissionMode,
) -> houyicoder_protocol::frontend::permission::PermissionMode {
    use houyicoder_permission::PermissionMode as M;
    use houyicoder_protocol::frontend::permission::PermissionMode as W;
    match mode {
        M::Manual => W::Manual,
        M::Auto => W::Auto,
    }
}

/// Inverse of permission_mode_to_wire: take a wire PermissionMode back to the
/// engine form the gate stores. The server is the single write authority for
/// mode; the frontend never names the engine PermissionMode.
pub(crate) fn permission_mode_from_wire(
    mode: houyicoder_protocol::frontend::permission::PermissionMode,
) -> houyicoder_permission::PermissionMode {
    use houyicoder_permission::PermissionMode as M;
    use houyicoder_protocol::frontend::permission::PermissionMode as W;
    match mode {
        W::Manual => M::Manual,
        W::Auto => M::Auto,
        // non_exhaustive: an unknown wire variant fails safe to Manual.
        _ => M::Manual,
    }
}

/// Build a Yes-don't-ask consent rule when the human approves with scope
/// "always". Bash-family tools scope to a command prefix (compound/destructive
/// commands → None, approved this once only); the skill tool scopes to the
/// skill name at Local scope; other tools get a content-less rule.
pub(crate) fn consent_rule_for(
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<houyicoder_permission::Rule> {
    use houyicoder_permission::{Effect, Rule, RuleContent, Scope};
    let is_bash = matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "bash" | "sh" | "exec" | "shell"
    );
    if is_bash {
        let command = houyicoder_permission::input_content(tool_name, Some(input));
        let prefix = houyicoder_permission::bash_always_allow_prefix(&command)?;
        Rule::with_content(tool_name, RuleContent::Prefix(prefix), Effect::Allow).ok()
    } else if tool_name.eq_ignore_ascii_case("skill") {
        // Scope to the specific skill name, not a blanket tool-level rule, and
        // land at Local (machine-local) so a skill approval never travels
        // with the repo. A missing skill name installs nothing durable.
        let skill = input.get("skill").and_then(|v| v.as_str())?;
        Rule::with_content(
            tool_name,
            RuleContent::Exact(skill.to_string()),
            Effect::Allow,
        )
        .ok()
        .map(|r| r.with_scope(Scope::Local))
    } else {
        Rule::new(tool_name, Effect::Allow).ok()
    }
}

/// Map a wire rule back to the engine form at the service boundary so the
/// server applies exactly the rule the frontend authored — including a bash
/// prefix-scoped content rule, not the blanket tool-allow the server would
/// otherwise reconstruct, and the rule's persistence scope (destination).
/// Inverse of permission_rule_to_wire; the wire path is the single write
/// authority.
pub(crate) fn permission_rule_from_wire(
    rule: &houyicoder_protocol::frontend::permission::PermissionRule,
) -> Result<houyicoder_permission::Rule, houyicoder_permission::ModeError> {
    use houyicoder_permission::{Effect, Rule, RuleContent};
    use houyicoder_protocol::frontend::permission::{PermissionEffect, PermissionRuleContent};
    let effect = match rule.effect {
        PermissionEffect::Allow => Effect::Allow,
        PermissionEffect::Reject => Effect::Deny,
        PermissionEffect::Ask => Effect::Ask,
    };
    let content = rule.content.as_ref().map(|c| match c {
        PermissionRuleContent::Exact { value } => RuleContent::Exact(value.clone()),
        PermissionRuleContent::Prefix { value } => RuleContent::Prefix(value.clone()),
        PermissionRuleContent::Glob { value } => RuleContent::Glob(value.clone()),
    });
    let scope = wire_destination_to_scope(rule.destination);
    let rule = match content {
        Some(c) => Rule::with_content(&rule.action, c, effect)?,
        None => Rule::new(&rule.action, effect)?,
    };
    Ok(rule.with_scope(scope))
}

/// Map a durable engine rule to the wire form, including its persistence
/// scope (destination) so the /permissions Add flow's pick round-trips.
pub(crate) fn permission_rule_to_wire(
    rule: &houyicoder_permission::Rule,
) -> houyicoder_protocol::frontend::permission::PermissionRule {
    use houyicoder_permission::{Effect, RuleContent};
    use houyicoder_protocol::frontend::permission::{
        PermissionEffect, PermissionRule, PermissionRuleContent,
    };
    let effect = match rule.effect {
        Effect::Allow => PermissionEffect::Allow,
        Effect::Deny => PermissionEffect::Reject,
        Effect::Ask => PermissionEffect::Ask,
    };
    let content = rule.content.as_ref().map(|c| match c {
        RuleContent::Exact(v) => PermissionRuleContent::Exact { value: v.clone() },
        RuleContent::Prefix(v) => PermissionRuleContent::Prefix { value: v.clone() },
        RuleContent::Glob(v) => PermissionRuleContent::Glob { value: v.clone() },
    });
    PermissionRule {
        action: rule.action.clone(),
        content,
        effect,
        destination: scope_to_wire_destination(rule.scope),
    }
}

/// Map a wire destination to the engine persistence scope. Identity: user,
/// project, local.
fn wire_destination_to_scope(
    d: houyicoder_protocol::frontend::permission::RuleDestination,
) -> houyicoder_permission::Scope {
    use houyicoder_permission::Scope;
    use houyicoder_protocol::frontend::permission::RuleDestination;
    match d {
        RuleDestination::User => Scope::User,
        RuleDestination::Project => Scope::Project,
        RuleDestination::Local => Scope::Local,
        RuleDestination::Session => Scope::Session,
        RuleDestination::Builtin => Scope::Builtin,
    }
}

/// Inverse of wire_destination_to_scope.
fn scope_to_wire_destination(
    s: houyicoder_permission::Scope,
) -> houyicoder_protocol::frontend::permission::RuleDestination {
    use houyicoder_permission::Scope;
    use houyicoder_protocol::frontend::permission::RuleDestination;
    match s {
        Scope::User => RuleDestination::User,
        Scope::Project => RuleDestination::Project,
        Scope::Local => RuleDestination::Local,
        Scope::Session => RuleDestination::Session,
        Scope::Builtin => RuleDestination::Builtin,
    }
}

/// Map a session event to its session/update form. Standard event kinds map
/// one-to-one; audit-only kinds (meta user, compaction boundary, summary,
/// permission decision) return None and ride the acpx/context/* stream.
/// Streaming deltas return None (subsumed by the authoritative
/// AssistantMessage at turn end).
pub fn map_session_update(kind: &SessionEvent) -> Option<SessionUpdate> {
    let text_chunk = |text: &str| {
        ContentChunk::new(ContentBlock::Text {
            text: text.to_string(),
        })
    };
    if matches!(kind, SessionEvent::RewardObservation { .. }) {
        return None;
    }
    Some(match kind {
        SessionEvent::UserInput { text } => SessionUpdate::UserMessageChunk(text_chunk(text)),
        SessionEvent::MidTurnInput { text, .. } => {
            SessionUpdate::UserMessageChunk(text_chunk(text))
        }
        // A child-completion notification surfaces in the transcript so the
        // user sees what the model was told (the durable kind distinguishes
        // it from a user interjection; the visual chunk is the text summary).
        SessionEvent::NotificationInjected { summary, .. } => {
            SessionUpdate::UserMessageChunk(text_chunk(summary))
        }
        // The thinking field is a convenience folded from sibling
        // Reasoning events; the wire streams those as AgentThoughtChunk
        // separately, so the message chunk carries text only.
        SessionEvent::AssistantMessage { text, .. } => {
            SessionUpdate::AgentMessageChunk(text_chunk(text))
        }
        SessionEvent::Reasoning { text } => SessionUpdate::AgentThoughtChunk(text_chunk(text)),
        SessionEvent::ToolCall {
            call_id,
            tool,
            input,
        } => SessionUpdate::ToolCall(
            ToolCall::new(call_id.clone(), tool.clone())
                .raw_input(input.clone())
                .status(ToolCallStatus::InProgress),
        ),
        SessionEvent::ToolResult {
            call_id, output, ..
        } => SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            call_id.clone(),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .raw_output(output.clone()),
        )),
        SessionEvent::AssistantTextDelta { .. }
        | SessionEvent::MetaUser { .. }
        | SessionEvent::MemoryRecall { .. }
        | SessionEvent::SkillListing { .. }
        | SessionEvent::SkillBody { .. }
        | SessionEvent::CompactionBoundary { .. }
        | SessionEvent::Summary { .. }
        | SessionEvent::PermissionDecision { .. }
        | SessionEvent::TruncationVerdict { .. }
        | SessionEvent::WorktreeEnter { .. }
        | SessionEvent::WorktreeExit { .. }
        | SessionEvent::TurnUsage { .. }
        | SessionEvent::HookSignal { .. }
        | SessionEvent::TurnStarted { .. }
        | SessionEvent::CacheBreak { .. }
        | SessionEvent::SubagentSpawn { .. }
        | SessionEvent::SubagentReturn { .. } => return None,
        // TurnAborted is the user-visible boundary marker: map it as a
        // message chunk so the host renders the notice. The model-input
        // assembler skips it (the partial turn events are already there).
        SessionEvent::TurnAborted { reason } => {
            let notice = format!("previous turn was interrupted ({reason}), regenerated");
            SessionUpdate::UserMessageChunk(text_chunk(&notice))
        }
        SessionEvent::RewardObservation { .. } => return None,
        SessionEvent::Unknown => return None,
    })
}

/// Map a session event to its acpx/context/* extension notification. Covers
/// the audit kinds with no standard session/update variant; kinds that
/// already map to session/update return None. Params carry the event's serde
/// shape so a client reconstructs the typed payload.
pub(crate) fn map_acpx_notification(kind: &SessionEvent) -> Option<AcpxNotification> {
    use AcpxMethod::*;
    Some(match kind {
        SessionEvent::MetaUser { text } => {
            AcpxNotification::new(ContextMetaUser, serde_json::json!({ "text": text }))
        }
        SessionEvent::CompactionBoundary { checkpoint } => AcpxNotification::new(
            ContextCompactionBoundary,
            serde_json::json!({ "checkpoint": checkpoint.to_string() }),
        ),
        SessionEvent::Summary { text } => {
            AcpxNotification::new(ContextSummary, serde_json::json!({ "text": text }))
        }
        SessionEvent::PermissionDecision {
            call_id,
            tool,
            verdict,
            scope,
        } => AcpxNotification::new(
            ContextPermissionDecision,
            serde_json::json!({
                "callId": call_id,
                "tool": tool,
                "verdict": verdict.label(),
                "scope": scope,
            }),
        ),
        SessionEvent::TurnAborted { .. } => return None,
        SessionEvent::TruncationVerdict { .. }
        | SessionEvent::WorktreeEnter { .. }
        | SessionEvent::WorktreeExit { .. }
        | SessionEvent::TurnUsage { .. }
        | SessionEvent::HookSignal { .. }
        | SessionEvent::TurnStarted { .. }
        | SessionEvent::CacheBreak { .. }
        | SessionEvent::SubagentSpawn { .. }
        | SessionEvent::SubagentReturn { .. }
        | SessionEvent::NotificationInjected { .. } => return None,
        SessionEvent::UserInput { .. }
        | SessionEvent::MidTurnInput { .. }
        | SessionEvent::MemoryRecall { .. }
        | SessionEvent::SkillListing { .. }
        | SessionEvent::SkillBody { .. }
        | SessionEvent::AssistantMessage { .. }
        | SessionEvent::AssistantTextDelta { .. }
        | SessionEvent::ToolCall { .. }
        | SessionEvent::ToolResult { .. }
        | SessionEvent::Reasoning { .. }
        | SessionEvent::RewardObservation { .. } => return None,
        SessionEvent::Unknown => return None,
    })
}

/// The four verdict options the agent offers on every permission ask. The
/// option ids are stable strings so the server maps a selected id back to an
/// approved/rejected verdict without stashing the offered list per ask. The
/// names track the wire wording so a stock client renders them verbatim.
pub(crate) fn standard_permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: "allow_once".into(),
            name: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
            meta: None,
        },
        PermissionOption {
            option_id: "allow_always".into(),
            name: "Always allow".into(),
            kind: PermissionOptionKind::AllowAlways,
            meta: None,
        },
        PermissionOption {
            option_id: "reject_once".into(),
            name: "Reject once".into(),
            kind: PermissionOptionKind::RejectOnce,
            meta: None,
        },
        PermissionOption {
            option_id: "reject_always".into(),
            name: "Always reject".into(),
            kind: PermissionOptionKind::RejectAlways,
            meta: None,
        },
    ]
}

/// Map an engine approval request to the ACP reverse-request shape the agent
/// sends to the client mid-turn. The tool call under review rides a
/// ToolCallUpdate (call id plus raw input); the options are the four standard
/// verdicts. session_id is the display string of the session the ask is for.
pub(crate) fn approval_to_acp_permission(
    req: &houyicoder_core::agent::ApprovalRequest,
    session_id: String,
) -> RequestPermissionRequest {
    let tool_call = ToolCallUpdate {
        tool_call_id: req.call_id.clone().into(),
        fields: ToolCallUpdateFields {
            raw_input: Some(req.input.clone()),
            ..Default::default()
        },
    };
    RequestPermissionRequest {
        session_id,
        tool_call,
        options: standard_permission_options(),
        meta: None,
    }
}

/// Map the client's permission response back to the engine decision the
/// resume path consumes. Selected maps the option id to approved/rejected
/// (allow options approve; reject options deny). Cancelled is a reap (the run
/// was cancelled, not answered) — treat as denied so the tool sees a veto and
/// the turn can end. updated_input is None in the first cut: the ACP outcome
/// carries a verdict only, not an edited input (the answer-populated input
/// path is a frontend-dialect feature that lands with acpx/elicitation).
pub(crate) fn acp_permission_response_to_decision(
    resp: RequestPermissionResponse,
    call_id: String,
) -> houyicoder_core::agent::ApprovalDecision {
    let approved = match resp.outcome {
        RequestPermissionOutcome::Cancelled => false,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
            matches!(option_id.as_str(), "allow_once" | "allow_always")
        }
    };
    houyicoder_core::agent::ApprovalDecision {
        call_id,
        approved,
        updated_input: None,
    }
}

#[cfg(test)]
#[path = "protocol_adapter_tests.rs"]
mod tests;
