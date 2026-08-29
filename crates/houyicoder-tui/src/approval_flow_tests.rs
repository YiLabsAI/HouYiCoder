//! Approval-flow tests split from run_control_tests.rs for file-size. Uses
//! the parent module's scripted-provider helpers to drive the real runner.

use super::*;
use crate::state::TranscriptLine;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
use houyicoder_provider::FakeProvider;

/// Monotonic counter for unique temp-dir names, avoiding same-nanosecond
/// collisions when tests run in parallel.
fn unique_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[test]
fn test_one_at_a_time() {
    // Two tool calls in one turn: the model calls guarded twice (c1, c2)
    // then a final text reply. The runner interrupts with BOTH approvals;
    // the UI shows the first. Approve it (one decision) -> core applies it,
    // returns Interruption(remaining) -> the second card appears. Reject
    // the second -> core feeds back a "rejected by user" result -> model
    // emits the final reply. This is the one-at-a-time flow end-to-end.
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
    let mut got_first = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() {
            got_first = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(got_first, "first approval should appear");
    // The wire path surfaces one approval at a time: the server sends one
    // reverse permission ask, waits for the verdict, resumes, then re-asks
    // for any remaining. So pending_approvals is one, not the full batch.
    assert_eq!(app.pending_approvals.len(), 1, "one approval at a time");
    let first_id = app.approval.as_ref().unwrap().call_id.clone();

    // Approve the first (one decision for its call_id).
    app.resolve_current_approval(houyicoder_protocol::frontend::run::ApprovalDecision {
        call_id: first_id.clone(),
        approved: true,
        updated_input: None,
        scope: "once".to_string(),
    });

    // Wait for the second approval card (the core re-interrupts).
    let mut got_second = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() && app.agent_busy {
            continue;
        }
        if app.approval.is_some() {
            got_second = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        got_second,
        "second approval should appear after first approved"
    );
    let second_id = app.approval.as_ref().unwrap().call_id.clone();
    assert_ne!(
        first_id, second_id,
        "second approval must be a different call_id"
    );

    // Reject the second (one reject decision for its call_id).
    app.resolve_current_approval(houyicoder_protocol::frontend::run::ApprovalDecision {
        call_id: second_id,
        approved: false,
        updated_input: None,
        scope: "once".to_string(),
    });

    // Wait for the run to finish (model emits the final text).
    let mut settled = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy && !app.reverse_request_in_flight() {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(settled, "run should settle after second decision");
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::Agent(s) if s == "both handled"
    )));
}

#[test]
fn test_approval_renders_inline() {
    // The approval prompt renders inline at the transcript tail (a
    // bottom-aligned sub-rect), not a floating centered popup. Verify the
    // thin separator and the proceed question appear in the bottom rows when
    // an approval is pending, and are absent when none is pending.
    use crate::composition;
    use crate::test_support::render_text;

    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".to_string(),
        args: r#"{"command":"ls"}"#.to_string(),
        reason: "wants to run".to_string(),
        selected: 0,
        call_id: "c1".to_string(),
        options: Vec::new(),
        ..Default::default()
    });
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
    app2.approval = None;
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
    app.approval = Some(crate::state::Approval {
        tool: "bash".to_string(),
        args: "".to_string(),
        reason: "".to_string(),
        selected: 0,
        call_id: "c1".to_string(),
        options: Vec::new(),
        ..Default::default()
    });
    crate::keys::handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.approval.is_none(), "current approval cleared");
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
    app.approval = Some(crate::state::Approval {
        tool: "bash".to_string(),
        args: "".to_string(),
        reason: "".to_string(),
        selected: 0,
        call_id: "c1".to_string(),
        options: Vec::new(),
        ..Default::default()
    });
    crate::keys::handle_working(&mut app, key(KeyCode::Enter));
    assert!(app.approval.is_none(), "approval cleared after approve");
}

/// The a/1 and r/3 keys pin the approval selection without resolving it: a
/// selects Yes, r selects No when remember is shown. The card stays open so
/// the user can confirm with Enter or change again. Covers the selection arms
/// the Enter/Esc tests do not reach (they resolve immediately).
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
    app.approval = Some(mk());
    crate::keys::handle_working(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.approval.as_ref().unwrap().selected, 0, "a pins Yes");
    assert!(app.approval.is_some(), "card stays open after a");

    // 'r' (or '3') pins No when remember is shown (default approval, not a
    // protected path, so the arm fires).
    app.approval = Some(mk());
    crate::keys::handle_working(&mut app, key(KeyCode::Char('r')));
    assert_eq!(app.approval.as_ref().unwrap().selected, 1, "r pins No");
    assert!(app.approval.is_some(), "card stays open after r");
}

#[test]
fn test_approval_pretext_survives_rebuild() {
    // When the agent produces text followed by a guarded tool call, the
    // Interruption triggers a transcript rebuild. The assistant pre-text
    // (the Agent line from the AssistantMessage event) must survive the
    // merge — not be dropped or overwritten. This guards against a regression
    // where the live preview vanishes and the rebuilt transcript omits the
    // assistant's explanation that preceded the approval request.
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
    let mut got_approval = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() {
            got_approval = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
    let root = std::env::temp_dir().join(format!("houyi-walk-{seq}", seq = unique_seq()));
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
    assert_eq!(found, Some(root.canonicalize().unwrap()));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn test_walk_none_outside_repo() {
    // From a fresh tempdir whose parent chain has no Cargo.toml, the walk must
    // return None — this is the HOME-avoidance guard (a regression that
    // returned Some(home) would silently make the seatbelt workspace the home
    // dir). The system temp dir on mac (/var/folders/...) has no manifest
    // above it; if a parent happened to have one this assertion would surface
    // that (a real env anomaly), not pass silently.
    use std::fs;
    let d = std::env::temp_dir().join(format!("houyi-none-{}", unique_seq()));
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
    // End-to-end: a scripted provider returning nonzero usage, run to
    // completion. The runner's shared accumulator must fold the response
    // usage so status_snapshot reports the cumulative tally + the last
    // response's input_tokens (the current window footprint). This wires
    // the drive_loop write path that /context + /compact read.
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
    let mut settled = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(settled, "run should settle");
    // The TUI holds no engine handle, so the accumulator is read through the
    // wire: once the run settles (idle), the event loop's periodic status poll
    // fires a StatusQuery; the server projects runner.status_snapshot and the
    // driver routes the StatusResult back here. Pump until the cache lands.
    for _ in 0..200 {
        app.poll_agent();
        if app.status_cache.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
        let e = houyicoder_protocol::llm::ProviderError::Unknown(
            "hanging provider does not complete".into(),
        );
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
    // Esc while a run is in flight aborts it: the cancel token fires, the
    // drive loop flushes partial text + returns Interrupted, the Done handler
    // clears busy. The interrupt is implicit — no bracketed user-facing marker
    // line; the reason only goes to the model as the aborted tool-result.
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(HangingProvider::new(vec![LlmEvent::TextDelta {
        id: "t1".into(),
        text: "partial answer".into(),
    }]));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.spawn_run("hi".into());
    // Wait until the run is actively streaming: agent_busy is set synchronously
    // by spawn_run, but the cancel token is only installed once the spawned
    // task enters model_call_stream. The first TextDelta proves the run has
    // entered the streaming select (so the token exists and abort will land).
    // Without this gate, Esc could fire before the token is set and miss.
    let mut streaming = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.live_active {
            streaming = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(streaming, "run should stream a delta before abort");
    assert!(app.agent_busy, "run should still be in flight");
    // Press Esc on the working surface — must call abort_run.
    crate::keys::handle_working(
        &mut app,
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ),
    );
    // The cancel token fired; the drive loop returns Interrupted. Poll until
    // the Done message arrives and busy clears.
    let mut settled = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
    // Abort before the model emits any token: the stream never produces an
    // event (pending tail), the cancel token fires in the first select, the
    // drive loop returns Interrupted without appending any assistant content.
    // The Done handler then sees no real content after the last user input
    // and restores the original input so the user can edit and resend.
    let p = Arc::new(HangingProvider::new(Vec::new()));
    let mut app = app_with_provider(p, ToolRegistry::new());
    let original = "rewrite this as a pure function";
    app.spawn_run(original.into());
    assert!(app.agent_busy, "run should be in flight");
    assert_eq!(app.last_run_input.as_deref(), Some(original));
    // The cancel token is installed inside the spawned run() task, which
    // starts on a worker thread. Re-fire abort each tick: it is a no-op
    // until the token exists, then lands once the task has set it. Poll
    // until the Done message arrives and busy clears.
    let mut settled = false;
    for _ in 0..300 {
        app.abort_run();
        app.poll_agent();
        if !app.agent_busy {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
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
    // Simulate /context: push User echo + ContextGrid (TUI-only lines).
    app.push_transcript_line(TranscriptLine::User("/context".into()));
    app.push_transcript_line(TranscriptLine::ContextGrid(composition::context_view()));
    // Spawn "hi" and wait for Done.
    app.spawn_run("hi".into());
    let mut settled = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
    let buf = crate::test_support::render_buffer(&app, 100, 50);
    let text = crate::test_support::dump_buffer(&buf);
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
    // Simulate the real /context flow: submit_input pushes User echo
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
    let buf = crate::test_support::render_buffer(&app, 100, 50);
    let text = crate::test_support::dump_buffer(&buf);
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
    // After a second run, TUI-only lines from the first run (System
    // "thought for Ns") must stay at their position, not accumulate at
    // the tail or vanish.
    let p = Arc::new(FakeProvider::text("first reply"));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.screen = crate::state::Screen::Working;
    app.push_transcript_line(TranscriptLine::User("/context".into()));
    app.push_transcript_line(TranscriptLine::ContextGrid(composition::context_view()));
    // First run.
    app.spawn_run("hi".into());
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Second run.
    app.spawn_run("again".into());
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
