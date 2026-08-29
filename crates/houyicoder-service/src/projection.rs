//! Engine-to-wire boundary projections. The server's protocol loop owns the
//! carrier and the half-live turn state machine; the mapping from engine
//! types to protocol wire types lives here so the two concerns stay apart
//! and server.rs does not exceed the file-size gate. Every function is a pure
//! boundary mapping — no I/O, no state — so it is trivially testable without
//! a carrier.

pub(crate) mod compact;
mod labels;
pub(crate) mod memory;
pub(crate) mod redundant;
pub(crate) mod session_meta;
use labels::{hex_short, trajectory_kind_label};

use houyicoder_context::TurnEventKind;
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
pub(crate) use session_meta::project_session_meta;

/// Project the engine run result to the protocol message form. Outcome
/// variants match the engine enum one-for-one except Interruption (a
/// mid-turn permission ask is a reverse request, not an outcome) and
/// VerifyFailure (collapses to a summary string).
pub(crate) fn project_run_result(run: &houyicoder_core::agent::RunResult) -> RunResult {
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
        // reverse-request + resume loop and only calls project_run_result on
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
) -> ApprovalRequest {
    ApprovalRequest {
        call_id: req.call_id.clone(),
        tool_name: req.tool_name.clone(),
        input: req.input.clone(),
        options: Vec::new(),
        reason: reason.map(houyicoder_protocol::frontend::permission::AskReason::from),
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

/// Project the engine runner status snapshot to the wire form. The engine
/// snapshot carries a borrowed breaker-state label and a Duration cool-down;
/// the wire form owns both (a String and a whole-second count) so the TUI
/// renders /status without importing the engine or resilience crate.
pub(crate) fn project_status(s: &houyicoder_core::agent::StatusSnapshot) -> WireStatusSnapshot {
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
        // Sidecar + env-config (meta, auth, base_url, setting_sources) attach
        // server-side; the engine snapshot has none. ..Default fills them.
        ..Default::default()
    }
}

/// Project the engine trajectory (a Vec of turn events) to the wire audit-log
/// form (a Vec of TrajectoryEntry). Unlike the live SessionUpdate stream which
/// carries only the chat render surface, the audit log keeps one row per event
/// across ALL kinds including those the base session/update has no standard
/// counterpart for (compaction boundary, summary, meta user, permission
/// decision), plus the event id and the prev_hash linking each event into the
/// append-only chain. The TUI renders /trajectory from this and can verify the
/// server is not dropping events. The kind label is rendered as a fixed-width
/// string at the TUI boundary.
pub(crate) fn project_trajectory(
    events: &[houyicoder_context::TurnEvent],
) -> Vec<houyicoder_protocol::frontend::trajectory::TrajectoryEntry> {
    use houyicoder_protocol::frontend::trajectory::TrajectoryEntry;
    events
        .iter()
        .map(|ev| {
            let duration_ms = match &ev.kind {
                houyicoder_context::TurnEventKind::ToolResult { duration_ms, .. } => {
                    Some(*duration_ms)
                }
                _ => None,
            };
            TrajectoryEntry {
                kind: trajectory_kind_label(&ev.kind).to_string(),
                ts: ev.ts,
                event_id: ev.id.to_string(),
                prev_hash: ev.prev_hash.as_ref().map(|h| hex_short(&h.0)),
                duration_ms,
            }
        })
        .collect()
}

/// Project the engine context-window breakdown to the wire form so the TUI
/// renders /context without importing the engine or context crate.
pub(crate) fn project_context_breakdown(
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
    // it stays in sync with the grid the projection just built (not a stale
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

/// Project a run failure to the protocol message form. The kind is the
/// variant name the frontend records; the message is the Display string it
/// surfaces as an error line.
pub(crate) fn project_run_error(e: &houyicoder_core::agent::RunError) -> RunError {
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

/// Project the engine permission mode to the wire form.
pub(crate) fn project_permission_mode(
    mode: houyicoder_permission::PermissionMode,
) -> houyicoder_protocol::frontend::permission::PermissionMode {
    use houyicoder_permission::PermissionMode as M;
    use houyicoder_protocol::frontend::permission::PermissionMode as W;
    match mode {
        M::Manual => W::Manual,
        M::Auto => W::Auto,
    }
}

/// Inverse of project_permission_mode: take a wire PermissionMode back to the
/// engine form the gate stores. The server is the single write authority for
/// mode; the frontend never names the engine PermissionMode.
pub(crate) fn wire_mode_to_engine(
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

/// Apply a Yes-don't-ask consent rule at the service boundary when the human
/// approves a tool call with scope "always". For bash-family tools the rule is
/// scoped to a command prefix (refusing compound/destructive commands → None,
/// so the call is approved this once only); for the skill tool the rule is
/// scoped to the specific skill name and lands at Local scope (machine-local,
/// not repo-shared) so one approval cannot pre-authorize every future skill
/// invocation for every collaborator; for other tools a content-less tool rule.
/// The server owns the approval (tool name + input Value), so the prefix is
/// computed here from the same data the engine raised the interruption with —
/// the frontend never imports the permission crate's prefix-scoping helpers.
/// Mirrors the frontend's former always_allow_rule, moved server-side so the
/// tui->permission dep closes.
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

/// Project a wire rule back to the engine form at the service boundary so
/// the server applies exactly the rule the frontend authored — including a
/// bash prefix-scoped content rule, not the blanket tool-allow the server
/// would otherwise reconstruct from a bare action string, and the rule's
/// persistence scope (destination). Inverse of project_permission_rule; the
/// wire path is the single write authority.
pub(crate) fn wire_rule_to_engine(
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

/// Project a durable engine rule to the wire form, including its persistence
/// scope (destination) so the /permissions Add flow's pick round-trips.
pub(crate) fn project_permission_rule(
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

/// Project an engine turn-event kind onto its ACP session/update form. Kinds
/// the base protocol has a standard variant for (user / agent / thought
/// message chunks, tool calls, tool-call updates) map one-to-one; kinds with
/// no base counterpart (meta user, compaction boundary, summary, permission
/// decision) return None here and ride the acpx/context/* stream instead.
/// Streaming assistant deltas return None too: they are the live audit trail
/// subsumed by the authoritative AssistantMessage that lands at turn end, so
/// the wire transcript never double-counts a streamed chunk (the live preview
/// rides the shared live sink, not the wire). A future kind with no mapping
/// returns None so the adapter drops it rather than inventing a wire shape.
pub fn project_session_update(kind: &TurnEventKind) -> Option<SessionUpdate> {
    let text_chunk = |text: &str| {
        ContentChunk::new(ContentBlock::Text {
            text: text.to_string(),
        })
    };
    if matches!(kind, TurnEventKind::RewardObservation { .. }) {
        return None;
    }
    Some(match kind {
        TurnEventKind::UserInput { text } => SessionUpdate::UserMessageChunk(text_chunk(text)),
        TurnEventKind::MidTurnInput { text } => SessionUpdate::UserMessageChunk(text_chunk(text)),
        // The thinking field is a projection convenience folded from sibling
        // Reasoning events; the wire streams those as AgentThoughtChunk
        // separately, so the message chunk carries text only.
        TurnEventKind::AssistantMessage { text, .. } => {
            SessionUpdate::AgentMessageChunk(text_chunk(text))
        }
        TurnEventKind::Reasoning { text } => SessionUpdate::AgentThoughtChunk(text_chunk(text)),
        TurnEventKind::ToolCall {
            call_id,
            tool,
            input,
        } => SessionUpdate::ToolCall(
            ToolCall::new(call_id.clone(), tool.clone())
                .raw_input(input.clone())
                .status(ToolCallStatus::InProgress),
        ),
        TurnEventKind::ToolResult {
            call_id, output, ..
        } => SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            call_id.clone(),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .raw_output(output.clone()),
        )),
        TurnEventKind::AssistantTextDelta { .. }
        | TurnEventKind::MetaUser { .. }
        | TurnEventKind::MemoryRecall { .. }
        | TurnEventKind::SkillListing { .. }
        | TurnEventKind::SkillBody { .. }
        | TurnEventKind::CompactionBoundary { .. }
        | TurnEventKind::Summary { .. }
        | TurnEventKind::PermissionDecision { .. }
        | TurnEventKind::TruncationVerdict { .. }
        | TurnEventKind::WorktreeEnter { .. }
        | TurnEventKind::WorktreeExit { .. }
        | TurnEventKind::TurnUsage { .. }
        | TurnEventKind::HookSignal { .. }
        | TurnEventKind::TurnStarted { .. }
        | TurnEventKind::CacheBreak { .. }
        | TurnEventKind::SubagentSpawn { .. }
        | TurnEventKind::SubagentReturn { .. }
        | TurnEventKind::NotificationInjected { .. } => return None,
        // TurnAborted is the user-visible boundary marker: project it as a
        // message chunk so the host renders the notice. The model-input
        // projection skips it (the partial turn events are already there).
        TurnEventKind::TurnAborted { reason } => {
            let notice = format!("previous turn was interrupted ({reason}), regenerated");
            SessionUpdate::UserMessageChunk(text_chunk(&notice))
        }
        TurnEventKind::RewardObservation { .. } => return None,
        TurnEventKind::Unknown => return None,
    })
}

/// Project an engine turn-event kind onto its acpx/context/* extension
/// notification. These are the durable-context audit kinds the base
/// session/update has no standard variant for; they ride the extension
/// stream so a standard client ignores them and an acpx client renders the
/// audit trail. Kinds with a standard session/update variant return None
/// here. The params carry the event's own fields serialized as the event's
/// serde shape so a client reconstructs the typed payload.
pub(crate) fn project_acpx_context(kind: &TurnEventKind) -> Option<AcpxNotification> {
    use AcpxMethod::*;
    Some(match kind {
        TurnEventKind::MetaUser { text } => {
            AcpxNotification::new(ContextMetaUser, serde_json::json!({ "text": text }))
        }
        TurnEventKind::CompactionBoundary { checkpoint } => AcpxNotification::new(
            ContextCompactionBoundary,
            serde_json::json!({ "checkpoint": checkpoint.to_string() }),
        ),
        TurnEventKind::Summary { text } => {
            AcpxNotification::new(ContextSummary, serde_json::json!({ "text": text }))
        }
        TurnEventKind::PermissionDecision {
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
        TurnEventKind::TurnAborted { .. } => return None,
        TurnEventKind::TruncationVerdict { .. }
        | TurnEventKind::WorktreeEnter { .. }
        | TurnEventKind::WorktreeExit { .. }
        | TurnEventKind::TurnUsage { .. }
        | TurnEventKind::HookSignal { .. }
        | TurnEventKind::TurnStarted { .. }
        | TurnEventKind::CacheBreak { .. }
        | TurnEventKind::SubagentSpawn { .. }
        | TurnEventKind::SubagentReturn { .. }
        | TurnEventKind::NotificationInjected { .. } => return None,
        TurnEventKind::UserInput { .. }
        | TurnEventKind::MidTurnInput { .. }
        | TurnEventKind::MemoryRecall { .. }
        | TurnEventKind::SkillListing { .. }
        | TurnEventKind::SkillBody { .. }
        | TurnEventKind::AssistantMessage { .. }
        | TurnEventKind::AssistantTextDelta { .. }
        | TurnEventKind::ToolCall { .. }
        | TurnEventKind::ToolResult { .. }
        | TurnEventKind::Reasoning { .. }
        | TurnEventKind::RewardObservation { .. } => return None,
        TurnEventKind::Unknown => return None,
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

/// Project an engine approval request to the ACP reverse-request shape the
/// agent sends to the client mid-turn. The tool call under review rides a
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

/// Project the client's permission response back to the engine decision the
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
#[path = "projection_tests.rs"]
mod tests;
