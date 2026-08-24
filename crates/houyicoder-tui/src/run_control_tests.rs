//! Integration tests for the agent-loop wiring: spawn_run /
//! resolve_current_approval / handle_agent_message, plus the
//! GuardedTool-through-real-runner mode-gate coverage. Split out of
//! run_control.rs so that file stays under the size gate.
//! the wire: a paired in-memory server drives runner.run, the TUI ships
//! MessageSend, and permission asks arrive as AgentMessage::PermissionAsk
//! reverse requests.
use super::*;
use crate::composition;
use crate::state::{Pane, TranscriptLine};
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_core::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::frontend::run::{ContentBlock, RunError, RunOutcome, RunResult};
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
use houyicoder_provider::FakeProvider;
use houyicoder_service::composition::walk_to_workspace_root;
use houyicoder_session::SessionStore;
use std::sync::Arc;
use std::sync::mpsc;
fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}
fn agent_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

/// Wire an App to a scripted provider + a paired in-memory server/client, so
/// the real runner.run path is driven end-to-end over the wire without a
/// network. tools lets the test register approval-requiring tools. Matches the
/// production composition root: the runner is shared (Arc) between the server
/// task and the TUI, and the driver task owns the client.
pub(super) fn app_with_provider(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> App {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let wire_session = houyicoder_protocol::frontend::SessionId(session.to_string());
    let runner = Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    let (tx, rx) = mpsc::channel::<AgentMessage>();
    let gate = Arc::new(houyicoder_permission::DefaultModeGate::new());
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let (runner, client, startup_warnings) =
        composition::pair_inproc_server(runner, session, gate, notify);
    drop(runner); // server owns the runner; the TUI holds no engine handle.
    composition::build_app(composition::RunnerBundle {
        client,
        agent_tx: tx,
        agent_rx: rx,
        session: wire_session,
        model: "test-model".to_string(),
        trajectory_log: None,
        export_log: None,
        snapshot: None,
        session_lister: None,
        skip_login: false,
        startup_warnings,
    })
}

/// A guarded tool that requires approval, for the Interruption path.
pub(super) struct Guarded;
impl Tool for Guarded {
    fn name(&self) -> &str {
        "guarded"
    }
    fn description(&self) -> &str {
        "needs approval"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        input: serde_json::Value,
    ) -> houyicoder_async::PFut<
        '_,
        Result<serde_json::Value, houyicoder_protocol::extension::ToolError>,
    > {
        Box::pin(async move { Ok(input) })
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// A destructive tool that records whether it ran, wrapped in a GuardedTool
/// so the real runner exercises the mode gate end-to-end (both Manual and Auto
/// pause for approval on a tool that declares it needs approval). Follows the
/// shape of the real bash/edit tools (destructive + requires approval).
struct BoomTool {
    ran: std::sync::Mutex<bool>,
}
impl BoomTool {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ran: std::sync::Mutex::new(false),
        })
    }
    fn ran(&self) -> bool {
        *self.ran.lock().expect("boom mutex")
    }
}
impl Tool for BoomTool {
    fn name(&self) -> &str {
        "boom"
    }
    fn description(&self) -> &str {
        "destructive test tool"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        input: serde_json::Value,
    ) -> houyicoder_async::PFut<
        '_,
        Result<serde_json::Value, houyicoder_protocol::extension::ToolError>,
    > {
        *self.ran.lock().expect("boom mutex") = true;
        Box::pin(async move { Ok(input) })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// Wire a BoomTool wrapped in a GuardedTool under the given mode, so the
/// real runner drives the gate. Returns the app (ready to spawn) and the
/// shared BoomTool handle to inspect whether execute ran.
fn app_with_guarded_tool(
    mode: houyicoder_permission::PermissionMode,
    responses: Vec<CompletionResponse>,
) -> (App, Arc<BoomTool>) {
    let boom = BoomTool::new();
    let gate: Arc<dyn houyicoder_permission::ModeGate> =
        Arc::new(houyicoder_permission::DefaultModeGate::with_mode(mode));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(houyicoder_permission::GuardedTool::new(
        boom.clone(),
        gate,
    )));
    let provider = Arc::new(FakeProvider::new(responses));
    (app_with_provider(provider, tools), boom)
}

/// One scripted ToolCall for "boom" then a final text reply.
fn boom_call_then_reply() -> Vec<CompletionResponse> {
    vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "boom".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]
}

#[test]
fn test_guarded_tool_manual_raises() {
    // Manual: boom needs approval -> Ask -> runner pauses with Interruption; popup shows, tool not run.
    let (mut app, boom) = app_with_guarded_tool(
        houyicoder_permission::PermissionMode::Manual,
        boom_call_then_reply(),
    );
    app.spawn_run("go".into());
    let mut raised = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() {
            raised = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(raised, "Manual should raise an approval popup");
    assert!(!boom.ran(), "the tool must not run before approval");
}

#[test]
fn test_guarded_tool_auto_raises() {
    // Auto mode still gates a tool that declares it needs approval: boom ->
    // Ask -> requires_approval true -> the runner pauses with an Interruption;
    // the popup shows, the tool has not run yet.
    let (mut app, boom) = app_with_guarded_tool(
        houyicoder_permission::PermissionMode::Auto,
        boom_call_then_reply(),
    );
    app.spawn_run("go".into());
    let mut raised = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() {
            raised = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(raised, "Auto should raise an approval popup");
    assert!(!boom.ran(), "the tool must not run before approval");
}

/// Cross-layer end-to-end regression gate (verify charter pillar 4): the
/// approval dispatch must cross the TUI to core to GuardedTool boundary and
/// actually execute the tool after a human Yes, not merely send a decision
/// and stop. This wires the real GuardedTool with the Default gate (not the
/// bare Guarded stub whose execute returns Ok), raises the popup, then
/// approves and asserts the inner tool ran and no approval-required error was
/// fed back as a result.
///
/// Was red before the execute_authorized bridge landed (a one-shot Yes sent an
/// approve decision but apply_decisions called plain execute, whose gate
/// re-check still saw Ask and returned the tool-requires-approval error, so
/// the tool never ran). Now green via execute_authorized, which honors the
/// human Yes (Ask proceeds) while a Deny still blocks. Keep this as the
/// regression gate for that bridge.
#[test]
fn test_approve_yes_executes_tool() {
    let (mut app, boom) = app_with_guarded_tool(
        houyicoder_permission::PermissionMode::Manual,
        boom_call_then_reply(),
    );
    app.spawn_run("go".into());
    let mut raised = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() {
            raised = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(raised, "Manual should raise an approval popup");

    // Approve THIS call (one-shot Yes) — the focused=0 path the user picks.
    let call_id = app.approval.as_ref().unwrap().call_id.clone();
    app.resolve_current_approval(houyicoder_protocol::frontend::run::ApprovalDecision {
        call_id,
        approved: true,
        updated_input: None,
        scope: "once".to_string(),
    });

    let mut settled = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy && !app.reverse_request_in_flight() {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(settled, "resume should settle after approval");

    // Hard assertions on terminal side effects, not dispatch shape.
    assert!(
        boom.ran(),
        "the tool MUST actually execute after a human Yes"
    );
    let leaked_error = app.transcript.iter().any(|l| match l {
        TranscriptLine::Tool { body, .. } => body.contains("tool requires approval"),
        _ => false,
    });
    assert!(
        !leaked_error,
        "the guarded-tool re-check error must not leak as the tool result"
    );
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::Agent(s) if s == "done"
    )));
}

/// Cross-layer negative path: a human reject must NOT execute the tool, and
/// the rejected-by-user error must feed back as the tool result so the model
/// can self-correct. Same real-GuardedTool wiring as the approve test so this
/// is a terminal-side-effect assertion, not a dispatch-shape one.
#[test]
fn test_reject_does_not_execute() {
    let (mut app, boom) = app_with_guarded_tool(
        houyicoder_permission::PermissionMode::Manual,
        boom_call_then_reply(),
    );
    app.spawn_run("go".into());
    let mut raised = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() {
            raised = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(raised, "Manual should raise an approval popup");

    let call_id = app.approval.as_ref().unwrap().call_id.clone();
    app.resolve_current_approval(houyicoder_protocol::frontend::run::ApprovalDecision {
        call_id,
        approved: false,
        updated_input: None,
        scope: "once".to_string(),
    });

    let mut settled = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy && !app.reverse_request_in_flight() {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(settled, "resume should settle after reject");
    assert!(!boom.ran(), "the tool MUST NOT run after a human reject");
    let rejected = app.transcript.iter().any(|l| match l {
        TranscriptLine::Tool { body, .. } => body.contains("rejected by user"),
        _ => false,
    });
    assert!(
        rejected,
        "the rejected-by-user error must feed back as the tool result"
    );
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::Agent(s) if s == "done"
    )));
}

#[test]
fn test_handle_final_output_refreshes() {
    let mut app = composition::app();
    // The driver ships each durable frame; App owns the history. Push the
    // frames before Done so the rebuild on Done reads them.
    app.handle_agent_message(AgentMessage::Frame(user_msg("hi")));
    app.handle_agent_message(AgentMessage::Frame(agent_msg("hello back")));
    let msg = AgentMessage::Done {
        result: Ok(RunResult {
            outcome: RunOutcome::FinalOutput {
                content: vec![ContentBlock::Text {
                    text: "hello back".into(),
                }],
            },
            turns: 1,
            usage: Usage {
                total_tokens: 42,
                ..Usage::default()
            },
            stop_reason: houyicoder_protocol::frontend::run::StopReason::EndTurn,
        }),
    };
    app.handle_agent_message(msg);
    assert!(!app.agent_busy);
    assert_eq!(app.status.tokens, 42);
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::User(s) if s == "hi"
    )));
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::Agent(s) if s == "hello back"
    )));
}

#[test]
fn test_permission_ask_raises_popup() {
    let mut app = composition::app();
    let ask = ApprovalRequest {
        call_id: "c1".into(),
        tool_name: "bash".into(),
        input: serde_json::json!({"command": "ls"}),
        options: Vec::new(),
        reason: None,
    };
    app.handle_agent_message(AgentMessage::PermissionAsk {
        req_id: houyicoder_protocol::envelope::RequestId(1),
        ask,
    });
    assert!(app.approval.is_some());
    assert_eq!(app.pending_approvals.len(), 1);
    let a = app.approval.as_ref().unwrap();
    assert_eq!(a.tool, "bash");
    assert_eq!(a.call_id, "c1");
}

#[test]
fn test_handle_error_records_system() {
    let mut app = composition::app();
    let msg = AgentMessage::Done {
        result: Err(RunError {
            kind: "provider_exhausted".to_string(),
            message: "provider exhausted: rate limited".to_string(),
        }),
    };
    app.handle_agent_message(msg);
    assert!(!app.agent_busy);
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::System(s) if s.contains("agent error")
    )));
}

#[test]
fn test_spawn_run_final_output() {
    let p = Arc::new(FakeProvider::text("real reply"));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.spawn_run("hi".into());
    // Poll until the spawned task ships its Done message (streaming sends a
    // burst of Deltas first, then Done). Multi-thread runtime runs the task
    // on a worker; poll_agent drains the channel each tick.
    let mut got = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy {
            got = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(got, "agent message should arrive");
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::Agent(s) if s.contains("real reply")
    )));
    assert!(!app.agent_busy);
}

#[test]
fn test_spawn_run_interruption() {
    let resp = CompletionResponse {
        output: vec![
            OutputItem::Text {
                text: "running".into(),
            },
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "guarded".into(),
                input: serde_json::json!({}),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Guarded));
    let mut app = app_with_provider(p, tools);
    app.spawn_run("do it".into());
    let mut got = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() {
            got = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(got, "agent message should arrive");
    assert!(app.approval.is_some(), "popup raised");
    assert_eq!(app.pending_approvals.len(), 1);
}

/// Esc-abort before any real content rewinds the frame log past the user
/// echo and any partial turn content, so the transcript drops the user line
/// (the submit is undone) and the input can be restored for editing. Pins
/// the rewind half of the auto-restore-on-interrupt path.
#[test]
fn test_rewind_drops_user_echo() {
    let mut app = composition::app();
    app.transcript.push(TranscriptLine::User("hello".into()));
    app.frames.push(user_msg("hello"));
    app.frames.push(agent_msg("partial"));
    app.rewind_to_last_user_input();
    assert!(
        app.frames.is_empty(),
        "user echo + partial turn content dropped from frames"
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "hello")),
        "user echo dropped from transcript: {:?}",
        app.transcript
    );
}

#[test]
fn test_resume_after_approval() {
    // Turn 1: model calls guarded tool -> Interruption. Turn 2: final text.
    let responses = vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "guarded".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "all done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let p = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Guarded));
    let mut app = app_with_provider(p, tools);
    app.spawn_run("go".into());
    let mut got = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() {
            got = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(got && app.approval.is_some());
    // Approve the current approval (one decision for its call_id) and resume.
    let call_id = app.approval.as_ref().unwrap().call_id.clone();
    app.resolve_current_approval(houyicoder_protocol::frontend::run::ApprovalDecision {
        call_id,
        approved: true,
        updated_input: None,
        scope: "once".to_string(),
    });
    let mut got2 = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy {
            got2 = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(got2, "resume message should arrive");
    assert!(app.approval.is_none());
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::Agent(s) if s == "all done"
    )));
}

/// resolve_current_approval clears thinking_started_at (and live_block) so
/// the spinner does not read Thinking for up to 2s after a fast approval.
#[test]
fn test_resolve_clears_thinking_window() {
    use crate::state::enums::LiveBlock;
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::run::ApprovalDecision;
    let mut app = composition::app();
    app.pending_permission_req_id.set(Some(RequestId(1)));
    app.thinking_started_at = Some(std::time::Instant::now());
    app.live_block = LiveBlock::Thinking;
    app.resolve_current_approval(ApprovalDecision {
        call_id: "c1".into(),
        approved: true,
        updated_input: None,
        scope: "once".to_string(),
    });
    assert_eq!(app.live_block, LiveBlock::None);
    assert!(
        app.thinking_started_at.is_none(),
        "thinking window must clear on resolve"
    );
}

/// Slash commands that ship a wire query (context / status / model) take the
/// mint-and-send path only when a session is wired. Drives the
/// command.rs branches that were uncovered after the Session extraction.
#[test]
fn test_slash_queries_ship_wired() {
    use houyicoder_protocol::frontend::SlashCommand;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());

    app.run_command(SlashCommand::Context);
    // /context sends a ContextQuery; drain the reply (prospective
    // breakdown when no turn has run) + assert the grid lands.
    for _ in 0..200 {
        app.poll_agent();
        if app
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::ContextGrid(_)))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::ContextGrid(_))),
        "/context should land the breakdown grid"
    );

    app.run_command(SlashCommand::Status);
    assert!(
        app.pending_status_command,
        "Status sets the pending-command flag"
    );
    assert_eq!(
        app.pane,
        Pane::Status,
        "Status opens the Status pane (not a transcript dump)"
    );

    app.run_command(SlashCommand::Model);
    assert_eq!(app.pane, Pane::Model, "Model opens the Model pane");

    // /trajectory now opens a pane directly (mock data, no server query),
    // so it does not produce a "fetching" system line. The pane is the surface.
    app.run_command(SlashCommand::Trajectory);
    assert_eq!(app.pane, Pane::Trajectory);

    app.run_command(SlashCommand::Tools);
    assert_eq!(app.pane, Pane::Tools, "Tools opens the Tools pane");
    assert_eq!(Pane::Tools.label(), "tools");

    app.run_command(SlashCommand::Agents);
    assert_eq!(app.pane, Pane::Agents, "Agents opens the Agents pane");
    app.tab_cycle_mode();
}

/// A minimal tool so /tools has a positive-signal response (a non-empty
/// snapshot) — an empty registry would reply with an empty list, which cannot
/// be told apart from "no reply" by length alone.
struct MarkerTool;
impl Tool for MarkerTool {
    fn name(&self) -> &str {
        "marker"
    }
    fn description(&self) -> &str {
        "a registered tool"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        input: serde_json::Value,
    ) -> houyicoder_async::PFut<
        '_,
        Result<serde_json::Value, houyicoder_protocol::extension::ToolError>,
    > {
        Box::pin(async move { Ok(input) })
    }
}

/// /agents and /tools ship a wire query and land the server's reply on the
/// App fields (agent_directory / tool_entries), proving the in-proc
/// server round-trip completes for these pane-query commands — not just the
/// pane open. /agents goes None→Some; /tools (with a registered tool) goes
/// empty→non-empty. A break in the driver→server→dispatch chain fails here.
#[test]
fn test_agents_tools_round_trip() {
    use houyicoder_protocol::frontend::SlashCommand;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MarkerTool));
    let mut app = app_with_provider(provider, tools);

    app.run_command(SlashCommand::Agents);
    assert_eq!(app.pane, Pane::Agents);
    let mut landed = false;
    for _ in 0..300 {
        app.poll_agent();
        if app.agent_directory.is_some() {
            landed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        landed,
        "/agents query must round-trip: agent_directory should be set"
    );

    app.run_command(SlashCommand::Tools);
    assert_eq!(app.pane, Pane::Tools);
    let mut landed = false;
    for _ in 0..300 {
        app.poll_agent();
        if app.tool_entries.iter().any(|t| t.name == "marker") {
            landed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        landed,
        "/tools query must round-trip: the registered tool should appear"
    );
}

/// /memory pane d-action + /memory forget command both ship a MemoryForgetQuery
/// when a session is wired (the carrier-present branch). The pane action routes
/// the row's scope; the command form routes "auto". Pins the send sites so a
/// refactor that drops the scope field or reverts to the no-carrier branch
/// fails here.
#[test]
fn test_forget_ships_queries_wired() {
    use crate::agent_message::AgentMessage;
    use houyicoder_protocol::frontend::SlashCommand;
    use houyicoder_protocol::frontend::memory::MemorySummaryEntry;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.run_command(SlashCommand::Memory);
    // Seed a project-scope row so the cursor lands on a Some(row) + the
    // scope extraction runs (the no-carrier tests hit the None branch).
    app.handle_agent_message(AgentMessage::MemoryListResult {
        entries: vec![MemorySummaryEntry {
            key: "proj-gate".into(),
            description: "a project rule".into(),
            source: "project".into(),
            scope: "project".into(),
            mtime_secs: 0,
        }],
    });
    app.forget_memory_at_cursor();
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("forgetting"))),
        "pane d-action ships the forget query"
    );
    // Command form: /memory forget <key> ships with scope "auto" (the
    // command form has no row to read a scope from).
    app.run_tui_local_command("memory forget build-gate");
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("forgetting"))),
        "command form ships the forget query"
    );
}

/// /permission git on a wired app takes the server-present branch (mint +
/// ship the query), which the server-less tests skip.
#[test]
fn test_git_ops_ships_wired() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.input.set("/permissions git".to_string());
    app.submit_input();
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::System(s) if s.contains("ask before git operations")
    )));
}

#[cfg(test)]
#[path = "run_control_compact_tests.rs"]
mod compact_tests;

#[cfg(test)]
#[path = "run_control_resume_tests.rs"]
mod resume_progressive_tests;

/// The idle poll seeds mode_cache on the first tick (mode_cache starts None)
/// so the status-bar pill renders from session start. Exercises the idle
/// branch + the request_permission_mode send without asserting on the wire.
#[test]
fn test_idle_seeds_mode_query() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    // Fresh app: idle, no approval, no prior status poll -> the idle branch
    // fires request_status + request_permission_mode (mode_cache is None).
    app.poll_agent();
    // The queries ship to the driver; nothing to assert on the wire, but the
    // idle-poll branch executed without panic and minted request ids.
}

/// The /model pane Enter ships a ModelSwitch { model, effort, effort_toggled }
/// over the wire when a session is wired (the carrier-present branch). Pumps
/// the driver + the in-proc server round-trip so the ModelApplied reply lands
/// as an AgentMessage::ModelResult the no-op handler absorbs without error.
/// Pins the TUI-side wire plumbing: the outbound ModelSwitch->ModelSet mapping
/// and the inbound ModelResult->AgentMessage mapping, which the --lib lcov
/// gate sees (the integration model_wire test covers the server side only).
#[test]
fn test_model_switch_ships_wired() {
    use crate::state::Pane;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.pane = Pane::Model;
    app.set_model_at_cursor();
    assert_eq!(app.model_tier, "Default", "Enter applies the Default tier");
    assert_eq!(app.pane, Pane::Transcript, "Enter closes the pane");
    // Pump the driver round-trip: the ModelSwitch ships as a ModelSet, the
    // server applies it + replies ModelApplied, the driver forwards the
    // ModelResult. Drain until quiet; assert no request error surfaced.
    let mut saw_error = false;
    for _ in 0..200 {
        app.poll_agent();
        if app
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("error:")))
        {
            saw_error = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!saw_error, "ModelSwitch wire round-trip must not error");
}

#[cfg(test)]
#[path = "real_content_predicate_tests.rs"]
mod real_content_predicate_tests;
#[cfg(test)]
#[path = "rebuild_cap_tests.rs"]
mod rebuild_cap_tests;

#[cfg(test)]
#[path = "run_control_stream_tests.rs"]
mod run_control_stream_tests;

#[cfg(test)]
#[path = "context_command_tests.rs"]
mod context_command_tests;

#[cfg(test)]
#[path = "approval_flow_tests.rs"]
mod approval_flow_tests;

#[cfg(test)]
#[path = "cursor_priority_tests.rs"]
mod cursor_priority_tests;

#[cfg(test)]
#[path = "approval_render_tests.rs"]
mod approval_render_tests;
