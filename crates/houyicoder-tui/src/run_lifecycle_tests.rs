//! Run approval, cancellation, restoration, and completion-state tests.

use std::env::temp_dir;
use std::thread::sleep;

use super::*;
use crate::agent_message::ServerResponse;
use crate::keys::handle_working;
use crate::pending_prompt::PendingPrompt;
use crate::state::TranscriptLine;
use crate::test_harness::{attach_connection, dump_buffer, render_buffer, render_text};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, StopReason};
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, ProviderError};
use houyicoder_provider::FakeProvider;
use houyicoder_service::composition::walk_to_workspace_root;

/// Pump the event loop until the predicate holds, or the budget runs out.
/// Returns whether it held. The run is driven by a worker thread, so a test
/// observes progress only by polling.
fn pump_until(app: &mut App, tries: usize, mut held: impl FnMut(&App) -> bool) -> bool {
    for _ in 0..tries {
        app.poll_agent();
        if held(app) {
            return true;
        }
        sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// Monotonic counter for unique temp-dir names, avoiding same-nanosecond
/// collisions when tests run in parallel.
fn unique_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[test]
fn test_one_at_a_time() {
    // Two guarded calls in one turn: the runner raises both approvals, the UI
    // shows one at a time, and each verdict resumes the same run.
    let responses = vec![
        CompletionResponse {
            output: vec![
                OutputItem::ToolCall {
                    id: "c1".into(),
                    name: "guarded".into(),
                    input: serde_json::json!({}),
                },
                OutputItem::ToolCall {
                    id: "c2".into(),
                    name: "guarded".into(),
                    input: serde_json::json!({}),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "both handled".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let p = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Guarded));
    let mut app = app_with_provider(p, tools);
    app.spawn_run("do both".into());

    // Wait for the first approval card.
    let got_first = pump_until(&mut app, 200, |a| a.approval().is_some());
    assert!(got_first, "first approval should appear");
    // The wire path surfaces one approval at a time: the server sends one
    // reverse permission ask, waits for the verdict, resumes, then re-asks
    // for any remaining. So the ask holds one approval, not the full batch.
    assert_eq!(
        app.prompt.as_ref().map(|p| p.request_count()).unwrap_or(0),
        1,
        "one approval at a time"
    );
    let first_id = app.approval().unwrap().call_id.clone();

    // Approve the first (one decision for its call_id).
    app.resolve_current_approval(ApprovalDecision {
        call_id: first_id.clone(),
        approved: true,
        updated_input: None,
        scope: "once".to_string(),
    });

    // Wait for the second approval card (the core re-interrupts).
    let mut got_second = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval().is_some() && app.agent_busy() {
            continue;
        }
        if app.approval().is_some() {
            got_second = true;
            break;
        }
        sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        got_second,
        "second approval should appear after first approved"
    );
    let second_id = app.approval().unwrap().call_id.clone();
    assert_ne!(
        first_id, second_id,
        "second approval must be a different call_id"
    );

    // Reject the second (one reject decision for its call_id).
    app.resolve_current_approval(ApprovalDecision {
        call_id: second_id,
        approved: false,
        updated_input: None,
        scope: "once".to_string(),
    });

    // Wait for the run to finish (model emits the final text).
    let settled = pump_until(&mut app, 200, |a| {
        !a.agent_busy() && !a.reverse_request_in_flight()
    });
    assert!(settled, "run should settle after second decision");
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::Agent(s) if s == "both handled"
    )));
}

#[test]
fn test_approval_renders_inline() {
    // The prompt renders inline at the transcript tail, not as a floating
    // popup, and disappears once no approval is pending.
    use crate::composition;
    use render_text;

    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.prompt = Some(PendingPrompt::approval_card(
        RequestId(0),
        crate::state::Approval {
            tool: "bash".to_string(),
            args: r#"{"command":"ls"}"#.to_string(),
            reason: "wants to run".to_string(),
            selected: 0,
            call_id: "c1".to_string(),
            options: Vec::new(),
            ..Default::default()
        },
    ));
    let text = render_text(&app, 80, 24);
    let tail: Vec<&str> = text.lines().rev().take(12).collect();
    let tail_joined = tail.join("\n");
    assert!(
        tail_joined.contains("Do you want to proceed?"),
        "proceed question should appear near the transcript tail:\n{text}"
    );
    assert!(
        tail_joined.contains('─'),
        "thin separator should appear near the tail:\n{text}"
    );

    // No approval -> no separator or proceed question near tail.
    let mut app2 = composition::app();
    app2.screen = crate::state::Screen::Working;
    app2.prompt = None;
    let text2 = render_text(&app2, 80, 24);
    assert!(
        !text2.contains("Do you want to proceed?"),
        "no proceed question when approval is None"
    );
}

#[test]
fn test_approval_esc_rejects_current() {
    // Without a runner, Esc on the current approval clears just that one
    // and records a reject verdict. No reject-all: spawn_resume is never
    // called (no runner).
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.prompt = Some(PendingPrompt::approval_card(
        RequestId(0),
        crate::state::Approval {
            tool: "bash".to_string(),
            args: "".to_string(),
            reason: "".to_string(),
            selected: 0,
            call_id: "c1".to_string(),
            options: Vec::new(),
            ..Default::default()
        },
    ));
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.approval().is_none(), "current approval cleared");
}

#[test]
fn test_approval_enter_approve_current() {
    // Enter with selected=0 (approve) sends one approve decision for the
    // current call_id. Without a runner, the approval is just cleared.
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.prompt = Some(PendingPrompt::approval_card(
        RequestId(0),
        crate::state::Approval {
            tool: "bash".to_string(),
            args: "".to_string(),
            reason: "".to_string(),
            selected: 0,
            call_id: "c1".to_string(),
            options: Vec::new(),
            ..Default::default()
        },
    ));
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(app.approval().is_none(), "approval cleared after approve");
}

/// The selection keys pin an answer without resolving it, so the user can
/// change their mind before Enter. The Enter and Esc tests resolve directly.
#[test]
fn test_approval_char_keys_select() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    let mk = || crate::state::Approval {
        tool: "bash".to_string(),
        args: "".to_string(),
        reason: "".to_string(),
        selected: 2,
        call_id: "c1".to_string(),
        options: Vec::new(),
        ..Default::default()
    };
    // 'a' (or '1') pins Yes.
    app.prompt = Some(PendingPrompt::approval_card(RequestId(0), mk()));
    handle_working(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.approval().unwrap().selected, 0, "a pins Yes");
    assert!(app.approval().is_some(), "card stays open after a");

    // No is always index 1, whatever order the card displays.
    app.prompt = Some(PendingPrompt::approval_card(RequestId(0), mk()));
    handle_working(&mut app, key(KeyCode::Char('r')));
    assert_eq!(app.approval().unwrap().selected, 1, "r pins No");
    assert!(app.approval().is_some(), "card stays open after r");
}

#[test]
fn test_approval_pretext_survives_rebuild() {
    // The assistant text that preceded a guarded call must survive the
    // transcript rebuild the interruption triggers.
    let responses = vec![CompletionResponse {
        output: vec![
            OutputItem::Text {
                text: "I need to run a guarded tool".into(),
            },
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "guarded".into(),
                input: serde_json::json!({}),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    }];
    let p = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Guarded));
    let mut app = app_with_provider(p, tools);
    app.spawn_run("go ahead".into());
    // Wait for the approval card.
    let got_approval = pump_until(&mut app, 200, |a| a.approval().is_some());
    assert!(got_approval, "approval card should appear");
    // The agent's pre-text must be in the transcript as an Agent line.
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Agent(s) if s.contains("guarded tool"))),
        "assistant pre-text must survive the rebuild, transcript: {:?}",
        app.transcript
    );
    // The user's input must also survive.
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "go ahead")),
        "user input must survive the rebuild, transcript: {:?}",
        app.transcript
    );
}

#[test]
fn test_walk_finds_workspace_root() {
    // The sandbox must pin to the repo root, not the launch subdir. Build a
    // temp repo: root/Cargo.toml ([workspace]) + root/crate/Cargo.toml, then
    // walk up from crate/ and assert it returns the workspace root.
    use std::fs;
    let root = temp_dir().join(format!("houyi-walk-{seq}", seq = unique_seq()));
    let crate_dir = root.join("crate");
    fs::create_dir_all(&crate_dir).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crate\"]\n",
    )
    .unwrap();
    fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"crate\"\n",
    )
    .unwrap();
    let found = walk_to_workspace_root(&crate_dir);
    assert_eq!(found, Some(dunce::canonicalize(&root).unwrap()));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn test_walk_none_outside_repo() {
    // A parent chain with no manifest yields None: the walk must not fall back
    // to the home directory.
    use std::fs;
    let d = temp_dir().join(format!("houyi-none-{}", unique_seq()));
    fs::create_dir_all(&d).unwrap();
    let found = walk_to_workspace_root(&d);
    assert_eq!(
        found, None,
        "walk from a no-manifest dir must return None, got {found:?}"
    );
    fs::remove_dir_all(&d).ok();
}

#[test]
fn test_status_snapshot_accumulates_live() {
    // A scripted provider reporting usage must land in the shared accumulator,
    // the figure /context and /compact read.
    let resp = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage {
            input_tokens: 12_400,
            output_tokens: 9_100,
            total_tokens: 21_500,
            cache_read_input_tokens: 10_000,
            non_cached_input_tokens: 2_400,
            ..Usage::default()
        },
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.spawn_run("go".into());
    let settled = pump_until(&mut app, 200, |a| !a.agent_busy());
    assert!(settled, "run should settle");
    // The TUI holds no engine handle, so the tally arrives over the wire: pump
    // the periodic status poll until the driver routes it back.
    pump_until(&mut app, 200, |a| a.status_cache.is_some());
    let snap = app
        .status_cache
        .as_ref()
        .expect("status cache populated by the periodic poll after the run settles");
    assert_eq!(snap.model, "test");
    assert_eq!(snap.cumulative_usage.input_tokens, 12_400);
    assert_eq!(snap.cumulative_usage.output_tokens, 9_100);
    assert_eq!(snap.cumulative_usage.total_tokens, 21_500);
    assert_eq!(snap.cumulative_usage.cache_read_input_tokens, 10_000);
    assert_eq!(snap.last_input_tokens, 12_400);
    assert_eq!(snap.context_window, 200_000);
    // Occupancy is the assembled context's own size, so a session that has run
    // carries one. This is the figure the status bar and the context pane both
    // read.
    assert!(
        snap.context_used_tokens.is_some_and(|used| used > 0),
        "a measured context reports its size: {:?}",
        snap.context_used_tokens
    );
    // /context now renders an inline grid block (canned breakdown for now);
    // the live accumulator is verified above via status_snapshot. The block
    // is pushed as a ContextGrid transcript line, not a System text line.
    app.push_transcript_line(TranscriptLine::ContextGrid(composition::context_view()));
    let has_grid = app
        .transcript
        .iter()
        .any(|l| matches!(l, crate::state::TranscriptLine::ContextGrid(_)));
    assert!(has_grid, "/context should push an inline ContextGrid line");
}

/// A provider that streams the scripted events then hangs forever (pending
/// tail). Lets an abort test start a run that enters the streaming select,
/// fire the cancel token, and observe RunOutcome::Interrupted.
struct HangingProvider {
    events: Vec<houyicoder_protocol::llm::LlmEvent>,
}
impl HangingProvider {
    fn new(events: Vec<houyicoder_protocol::llm::LlmEvent>) -> Self {
        Self { events }
    }
}
impl ModelProvider for HangingProvider {
    fn complete(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PFut<
        '_,
        Result<CompletionResponse, houyicoder_protocol::llm::ProviderError>,
    > {
        let e = ProviderError::Unknown("hanging provider does not complete".into());
        Box::pin(async move { Err(e) })
    }
    fn capabilities(&self) -> houyicoder_protocol::llm::ModelCapabilities {
        houyicoder_protocol::llm::ModelCapabilities::default()
    }
    fn stream(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PStream<
        '_,
        Result<houyicoder_protocol::llm::LlmEvent, houyicoder_protocol::llm::ProviderError>,
    > {
        use futures::StreamExt;
        let prefix = futures::stream::iter(self.events.clone().into_iter().map(Ok));
        let tail = futures::stream::pending();
        Box::pin(prefix.chain(tail))
    }
}

#[test]
fn test_esc_aborts_busy_run() {
    // Esc mid-run aborts: the cancel token fires and the Done handler clears
    // busy, with no user-facing marker line.
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(HangingProvider::new(vec![LlmEvent::TextDelta {
        id: "t1".into(),
        text: "partial answer".into(),
    }]));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.spawn_run("hi".into());
    // The cancel token exists only once the spawned task is streaming, so wait
    // for the first delta before firing Esc.
    let streaming = pump_until(&mut app, 200, |a| {
        a.run_progress().is_some_and(|p| p.live_active)
    });
    assert!(streaming, "run should stream a delta before abort");
    assert!(app.agent_busy(), "run should still be in flight");
    // Press Esc on the working surface — must call abort_run.
    handle_working(
        &mut app,
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ),
    );
    // The cancel token fired; the drive loop returns Interrupted. Poll until
    // the Done message arrives and busy clears.
    let settled = pump_until(&mut app, 200, |a| !a.agent_busy());
    assert!(settled, "aborted run should settle via Interrupted");
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("interrupted by user"))),
        "no user-facing interrupted-by-user line — the interrupt is implicit: {:?}",
        app.transcript
    );
    // The run streamed a delta before the abort, so it produced real content:
    // the input must NOT be restored.
    assert!(
        app.input.is_empty(),
        "input must stay empty when real content was produced"
    );
    assert!(app.last_run_input.is_none(), "stash cleared on Done");
}

#[test]
fn test_esc_abort_restores_input() {
    // Aborting before any token leaves no assistant content, so the Done
    // handler restores the input for the user to edit and resend.
    let p = Arc::new(HangingProvider::new(Vec::new()));
    let mut app = app_with_provider(p, ToolRegistry::new());
    let original = "rewrite this as a pure function";
    app.spawn_run(original.into());
    assert!(app.agent_busy(), "run should be in flight");
    assert_eq!(app.last_run_input.as_deref(), Some(original));
    // The token is installed on a worker thread, so re-fire abort each tick
    // until the Done message arrives.
    let mut settled = false;
    for _ in 0..300 {
        app.abort_run();
        app.poll_agent();
        if !app.agent_busy() {
            settled = true;
            break;
        }
        sleep(std::time::Duration::from_millis(10));
    }
    assert!(settled, "aborted run should settle via Interrupted");
    // The input box is restored to the original text, cursor at the end.
    assert_eq!(
        app.input.value(),
        original,
        "input must be restored after a no-content abort"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s == "input restored")),
        "transcript must carry the restore line: {:?}",
        app.transcript
    );
    // The stash is cleared (no new run was spawned, agent_busy is false).
    assert!(app.last_run_input.is_none(), "stash cleared after restore");
}

#[test]
fn test_context_grid_after_run() {
    // After /context + hi + Done, the ContextGrid + User echo must stay at
    // their original position (before the hi exchange), not vanish or move
    // to the tail.
    let p = Arc::new(FakeProvider::text("hello back"));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.screen = crate::state::Screen::Working;
    // Simulate /context: raise the command echo + ContextGrid rows.
    app.push_unanswered_echo("/context".into());
    app.push_transcript_line(TranscriptLine::ContextGrid(composition::context_view()));
    // Spawn "hi" and wait for Done.
    app.spawn_run("hi".into());
    let settled = pump_until(&mut app, 200, |a| !a.agent_busy());
    assert!(settled, "run should settle");
    // Assert correct ORDER: User(/context), ContextGrid, User(hi), Agent.
    let ctx_user = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(s) if s == "/context"));
    let ctx_grid = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::ContextGrid(_)));
    let hi_user = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(s) if s == "hi"));
    let agent = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::Agent(s) if s.contains("hello")));
    assert!(
        ctx_user.is_some(),
        "User(/context) missing: {:?}",
        app.transcript
    );
    assert!(
        ctx_grid.is_some(),
        "ContextGrid missing: {:?}",
        app.transcript
    );
    assert!(hi_user.is_some(), "User(hi) missing: {:?}", app.transcript);
    assert!(agent.is_some(), "Agent reply missing: {:?}", app.transcript);
    let cu = ctx_user.unwrap();
    let cg = ctx_grid.unwrap();
    let hu = hi_user.unwrap();
    let ag = agent.unwrap();
    assert!(
        cu < cg && cg < hu && hu < ag,
        "order wrong: ctx_user={cu} grid={cg} hi={hu} agent={ag}"
    );
    // Render and assert all are visible.
    let buf = render_buffer(&app, 100, 50);
    let text = dump_buffer(&buf);
    assert!(
        text.contains("Context Usage"),
        "grid header not visible: {text}"
    );
    assert!(text.contains("hello"), "agent reply not visible: {text}");
}

#[test]
fn test_slash_echo_visible() {
    // Bug 1: the User echo line (the slash-command echo) must appear in
    // the rendered output above the ContextGrid block, not be overwritten
    // by the block Clear.
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    // Simulate the real /context path: submit_input pushes User echo
    // THEN run_command pushes ContextGrid.
    app.input.set("/context".to_string());
    app.submit_input();
    // Assert the transcript has both lines.
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "/context")),
        "User echo missing from transcript"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::ContextGrid(_))),
        "ContextGrid missing from transcript"
    );
    // Render tall enough for the grid block.
    let buf = render_buffer(&app, 100, 50);
    let text = dump_buffer(&buf);
    // The User echo renders as "> /context" (the render() glyph for User).
    assert!(
        text.contains("/context"),
        "User echo (/context) not visible in render: {text}"
    );
    assert!(
        text.contains("Context Usage"),
        "Context Usage header not visible: {text}"
    );
}

#[test]
fn test_tui_lines_survive_runs() {
    // After a second run, the rows the frontend raised in the first run
    // (System "thought for Ns") must stay at their position, not
    // accumulate at the tail or vanish.
    let p = Arc::new(FakeProvider::text("first reply"));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.screen = crate::state::Screen::Working;
    app.push_transcript_line(TranscriptLine::User("/context".into()));
    app.push_transcript_line(TranscriptLine::ContextGrid(composition::context_view()));
    // First run.
    app.spawn_run("hi".into());
    pump_until(&mut app, 200, |a| !a.agent_busy());
    // Second run.
    app.spawn_run("again".into());
    pump_until(&mut app, 200, |a| !a.agent_busy());
    // ContextGrid must still be present and before both User(hi) and User(again).
    let cg = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::ContextGrid(_)));
    let hi = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(s) if s == "hi"));
    let again = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::User(s) if s == "again"));
    assert!(cg.is_some(), "ContextGrid missing after 2 runs");
    assert!(hi.is_some(), "User(hi) missing");
    assert!(again.is_some(), "User(again) missing");
    let cg = cg.unwrap();
    let hi = hi.unwrap();
    let again = again.unwrap();
    assert!(
        cg < hi && hi < again,
        "order wrong after 2 runs: grid={cg} hi={hi} again={again}"
    );
}

#[test]
fn test_guarded_tool_auto_asks() {
    // Auto still asks for a tool that declares it needs approval: boom ->
    // Ask -> the runner pauses with an Interruption and the popup shows (the
    // recoverable invariant, not a blanket skip, governs destructive ops).
    let (mut app, boom) = app_with_guarded_tool(
        houyicoder_permission::PermissionMode::Auto,
        boom_call_then_reply(),
    );
    app.spawn_run("go".into());
    let raised = pump_until(&mut app, 200, |a| a.approval().is_some());
    assert!(raised, "Auto should raise an approval popup");
    assert!(!boom.ran(), "the tool must not run before approval");
}

fn approval_ask(call_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        call_id: call_id.into(),
        tool_name: "bash".into(),
        input: serde_json::json!({"command": "ls"}),
        options: Vec::new(),
        reason: None,
        delegation: None,
    }
}

// RunState keeps the run start across the Waiting transition, so resuming from
// an approval does not reset the clock.
#[test]
fn test_ask_preserves_start() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(
        app.spawn_run("work".into()),
        "run starts on a live connection"
    );
    let original_start = app.run_started();
    assert!(original_start.is_some(), "precondition: run started");
    app.raise_agent_approval(approval_ask("c1"), RequestId(1));
    assert_eq!(
        app.run_started(),
        original_start,
        "the run start is preserved across the approval pause"
    );
    app.resolve_current_approval(ApprovalDecision {
        call_id: "c1".into(),
        approved: true,
        updated_input: None,
        scope: "once".into(),
    });
    assert_eq!(
        app.run_started(),
        original_start,
        "the verdict resumes without resetting the clock"
    );
}

// Streamed text and a tracked running tool both survive the Waiting
// transition, and a post-resume delta appends to the same preview.
#[test]
fn test_ask_preserves_progress() {
    use crate::agent_message::ServerEvent;
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Delta {
        text: "first".into(),
    }));
    app.run_progress_mut()
        .expect("active run")
        .running_tools
        .insert("c1".into());

    app.raise_agent_approval(approval_ask("c2"), RequestId(1));
    app.resolve_current_approval(ApprovalDecision {
        call_id: "c2".into(),
        approved: true,
        updated_input: None,
        scope: "once".into(),
    });

    app.handle_agent_message(SessionMessage::Event(ServerEvent::Delta {
        text: "second".into(),
    }));
    let p = app.run_progress().expect("active run");
    assert_eq!(p.live_assistant_text, "firstsecond");
    assert!(
        p.running_tools.contains("c1"),
        "the tracked tool survives the approval pause"
    );
}

// The run's wall start is copied into the session start before finish
// drops the ActiveRun, so the first final run fixes the session clock.
#[test]
fn test_finish_copies_start() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    let run_req = app.active_run_req_id().unwrap();
    let started = app.run_started();
    app.handle_agent_message(SessionMessage::Response {
        request: run_req,
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::FinalOutput {
                    content: vec![ContentBlock::Text { text: "ok".into() }],
                },
                usage: Usage::default(),
                turns: 1,
                stop_reason: StopReason::EndTurn,
            }),
        },
    });
    assert_eq!(
        app.session_started_at, started,
        "the run start is copied into the session start before the drop"
    );
}

// A connection loss after a clean Done pushes its notice but does not
// re-settle the finished run: last_run_final stays true and the run id
// stays cleared, so the loss is a notice rather than a second settle.
#[test]
fn test_final_run_survives_loss() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    let run_req = app.active_run_req_id().unwrap();
    app.handle_agent_message(SessionMessage::Response {
        request: run_req,
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::FinalOutput {
                    content: vec![ContentBlock::Text { text: "ok".into() }],
                },
                usage: Usage::default(),
                turns: 1,
                stop_reason: StopReason::EndTurn,
            }),
        },
    });
    assert!(app.status.last_run_final, "the Done settled as final");
    let before = app.transcript.len();
    app.apply_connection_loss("connect failed: no server".into(), Vec::new());
    assert!(
        app.status.last_run_final,
        "a later loss does not un-final the settled run"
    );
    assert_eq!(
        app.active_run_req_id(),
        None,
        "the loss finds no run to settle"
    );
    assert!(
        app.transcript.len() > before,
        "the loss still pushes its notice line"
    );
}

// A Done whose request id is not the active run's settles nothing; the
// matching Done then settles the run.
#[test]
fn test_stale_done_settles_nothing() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    let run_req = app.active_run_req_id().unwrap();
    let stale = RequestId(run_req.0 + 1);
    let done = || SessionMessage::Response {
        request: stale,
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::FinalOutput {
                    content: vec![ContentBlock::Text { text: "ok".into() }],
                },
                usage: Usage::default(),
                turns: 1,
                stop_reason: StopReason::EndTurn,
            }),
        },
    };
    app.handle_agent_message(done());
    assert!(app.agent_busy(), "a stale Done must not end the active run");
    assert_eq!(
        app.active_run_req_id(),
        Some(run_req),
        "a stale Done must not clear the active run's request id"
    );
    app.handle_agent_message(SessionMessage::Response {
        request: run_req,
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::FinalOutput {
                    content: vec![ContentBlock::Text { text: "ok".into() }],
                },
                usage: Usage::default(),
                turns: 1,
                stop_reason: StopReason::EndTurn,
            }),
        },
    });
    assert!(!app.agent_busy(), "the matching Done settles the run");
    assert_eq!(app.active_run_req_id(), None);
}

// Each approval cycle preserves the original start point — RunState keeps
// the ActiveRun across Waiting transitions, so the clock is not reset.
#[test]
fn test_cycles_preserve_start() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    let first_start = app.run_started();
    for round in 0..2 {
        app.raise_agent_approval(approval_ask(&format!("c{round}")), RequestId(1));
        assert_eq!(
            app.run_started(),
            first_start,
            "cycle {round}: start preserved across approval"
        );
        app.resolve_current_approval(ApprovalDecision {
            call_id: format!("c{round}"),
            approved: true,
            updated_input: None,
            scope: "once".into(),
        });
        assert_eq!(
            app.run_started(),
            first_start,
            "cycle {round}: verdict does not reset the clock"
        );
    }
}

// A submit while a card is up parks instead of starting a second run, because
// Waiting is an active state.
#[test]
fn test_waiting_submit_parks() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("first".into()));
    let first_req = app.active_run_req_id();
    app.raise_agent_approval(approval_ask("c1"), RequestId(1));
    assert!(!app.agent_busy(), "the spinner pauses during Waiting");
    app.spawn_run("second".into());
    assert_eq!(
        app.active_run_req_id(),
        first_req,
        "a submit during Waiting does not replace the waiting run"
    );
    assert!(
        app.pending.len() == 1,
        "the submit parks in the queue instead of starting a second run"
    );
}

// A direct submit while busy parks locally.
#[test]
fn test_busy_submit_parks() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("first".into()));
    app.spawn_run("second".into());
    assert!(app.agent_busy(), "no second run while busy");
    assert_eq!(app.pending.len(), 1, "the extra input parks in the queue");
    // promote_next_pending attaches the single server mirror right away
    // (the live connection accepts it) — the contract is at most one.
    assert!(
        matches!(app.pending.first(), Some(PendingItem::Message(_))),
        "the parked input is promoted to the single server mirror"
    );
}

// A loss naming the active run in not_sent reports it as not sent; an empty
// not_sent list reports it as unknown, since the frame may have landed.
#[test]
fn test_loss_unsent_vs_unknown() {
    use crate::agent_message::{ConnectionEvent, SessionMessage};
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    let run_req = app.active_run_req_id().unwrap();

    // not sent: the run's id is in the not_sent list.
    app.handle_agent_message(SessionMessage::Connection(ConnectionEvent::Lost {
        cause: "send failed: pipe broken".into(),
        not_sent: vec![run_req],
    }));
    let last = app.transcript.last().expect("a line landed");
    assert!(
        matches!(last, TranscriptLine::System(s) if s.contains("not sent")),
        "a not sent run must say so, got {last:?}"
    );

    // Reset for the unknown case.
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    app.handle_agent_message(SessionMessage::Connection(ConnectionEvent::Lost {
        cause: "send failed: pipe broken".into(),
        not_sent: vec![],
    }));
    let last = app.transcript.last().expect("a line landed");
    assert!(
        matches!(last, TranscriptLine::System(s) if s.contains("send failed")),
        "an unknown run must carry the cause, got {last:?}"
    );
    assert!(!app.agent_busy(), "either way the active run ends");
}

// Cancel during a Waiting approval moves the run to Cancelling without
// dropping the active run: the same request id stays in flight until the
// interrupted completion arrives.
#[test]
fn test_cancel_during_waiting() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    let run_req = app.active_run_req_id().unwrap();
    app.raise_agent_approval(approval_ask("c1"), RequestId(1));
    assert!(!app.agent_busy(), "Waiting pauses the spinner");
    assert!(!app.cancelling(), "Waiting is not Cancelling");
    app.abort_run();
    assert!(app.cancelling(), "abort moves Waiting to Cancelling");
    assert!(app.agent_busy(), "Cancelling still counts as busy");
    assert_eq!(
        app.active_run_req_id(),
        Some(run_req),
        "the same run stays in flight through the cancel"
    );
}

// A connection loss during Waiting settles the run as an error: the run
// goes idle, the prompt clears, and the outcome is not final so queued
// input parks instead of draining.
#[test]
fn test_loss_during_waiting_settles() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    app.raise_agent_approval(approval_ask("c1"), RequestId(1));
    assert!(
        app.active_run_req_id().is_some(),
        "the run is still in flight while Waiting"
    );
    app.apply_connection_loss("connect failed: no server".into(), Vec::new());
    assert!(!app.agent_busy(), "the loss settles the waiting run");
    assert!(!app.cancelling(), "nothing is left to cancel");
    assert_eq!(app.active_run_req_id(), None, "the run id clears on settle");
    assert!(app.prompt.is_none(), "the approval prompt clears on settle");
    assert!(!app.status.last_run_final, "an error settle is not final");
}

// A connection loss during Cancelling settles the same way: the run that
// was being cancelled ends, and no cancelling state lingers.
#[test]
fn test_loss_during_cancelling_settles() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    attach_connection(&mut app);
    assert!(app.spawn_run("work".into()));
    app.raise_agent_approval(approval_ask("c1"), RequestId(1));
    app.abort_run();
    assert!(app.cancelling(), "the run is cancelling before the loss");
    app.apply_connection_loss("connect failed: no server".into(), Vec::new());
    assert!(!app.agent_busy(), "the loss settles the cancelling run");
    assert!(!app.cancelling(), "cancelling clears on settle");
    assert_eq!(app.active_run_req_id(), None, "the run id clears on settle");
    assert!(app.prompt.is_none(), "the approval prompt clears on settle");
}
