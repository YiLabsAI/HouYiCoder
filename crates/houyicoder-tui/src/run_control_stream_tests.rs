//! Streaming tests split from run_control_tests.rs for the file-size gate.
//! Reasoning + text streaming persistence, and the output-tail truncation
//! guard. The two cover the live delta sink (ephemeral preview) versus the
//! authoritative AssistantMessage frame that lands on Done.
use super::*;

/// End-to-end: a provider returning OutputItem::Reasoning streams
/// ReasoningDelta through fold_event → the live sink → AgentMessage →
/// live_reasoning_text (transient), and the durable Reasoning event lands as a
/// TranscriptLine::Thinking after Done.
#[test]
fn test_reasoning_streams_and_persists() {
    let resp = CompletionResponse {
        output: vec![
            OutputItem::Reasoning {
                text: "pondering the task".into(),
            },
            OutputItem::Text {
                text: "here is my answer".into(),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.spawn_run("go".into());
    let mut got = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy {
            got = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(got, "run should settle");
    // The durable Reasoning event lands as a first-class Thinking line.
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Thinking { text } if text.contains("pondering"))),
        "durable Thinking line missing: {:?}",
        app.transcript
    );
    // And the assistant text follows it.
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::Agent(s) if s.contains("answer")
    )));
    // Live preview cleared after Done.
    assert!(app.live_reasoning_text.is_empty());
}

/// Regression guard for output-tail truncation. A streamed reply must land in
/// the transcript in full after Done — head and the last 4-char delta (the
/// tail). The live delta sink ships each chunk via try_send on a bounded
/// channel (an ephemeral preview the authoritative AssistantMessage frame
/// replaces on Done), so the rebuild from frames must carry every token. A
/// regression that drops the final chunk before the Finish event, or that lets
/// the live preview be cleared without the authoritative frame landing first,
/// would leave the tail missing. The reply is deliberately long (many deltas)
/// to stress the bounded live-delta channel.
#[test]
fn test_streamed_tail_survives_done() {
    let head = "HEADMARK the quick brown fox jumps over the lazy dog ";
    let middle: String =
        "alpha bravo charlie delta echo foxtrot golf hotel india juliet ".repeat(6);
    let tail = " and the final sentence ends here TAILMARK";
    let full = format!("{head}{middle}{tail}");
    let resp = CompletionResponse {
        output: vec![OutputItem::Text { text: full.clone() }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut app = app_with_provider(p, ToolRegistry::new());
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
    assert!(settled, "run should settle within the poll window");
    // The authoritative AssistantMessage frame must drive one Agent line whose
    // text is the full streamed reply. Concatenate every Agent line so a split
    // across lines (e.g. a length-recovery continuation) still passes.
    let agent_text = app
        .transcript
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::Agent(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(
        agent_text.contains("HEADMARK"),
        "head of streamed reply missing from transcript: {agent_text:?}"
    );
    assert!(
        agent_text.contains("TAILMARK"),
        "tail of streamed reply dropped — truncation regression: {agent_text:?}"
    );
    // The live preview must be cleared once the authoritative frame lands.
    assert!(
        app.live_assistant_text.is_empty(),
        "live preview should be cleared after Done"
    );
}

/// Esc aborting a run that already produced real content (agent text landed)
/// must surface a visible Interrupted row — the silent-Esc bug left no trace
/// in the transcript. A dim one-line interrupt marker.
#[test]
fn test_interrupted_content_shows_marker() {
    let mut app = composition::app();
    app.handle_agent_message(AgentMessage::Frame(user_msg("hi")));
    app.handle_agent_message(AgentMessage::Frame(agent_msg("partial reply")));
    let msg = AgentMessage::Done {
        result: Ok(RunResult {
            outcome: RunOutcome::Interrupted {
                reason: "user".into(),
            },
            turns: 1,
            usage: Usage::default(),
            stop_reason: houyicoder_protocol::frontend::run::StopReason::EndTurn,
        }),
    };
    app.handle_agent_message(msg);
    assert!(!app.agent_busy, "Interrupted clears busy");
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Interrupted)),
        "Interrupted lands a visible marker row even with real content"
    );
}

/// Esc aborting a run that produced no real content restores the input and
/// surfaces both the input-restored row and the Interrupted marker.
#[test]
fn test_interrupted_no_content_restores() {
    let mut app = composition::app();
    app.handle_agent_message(AgentMessage::Frame(user_msg("draft")));
    app.last_run_input = Some("draft".into());
    let msg = AgentMessage::Done {
        result: Ok(RunResult {
            outcome: RunOutcome::Interrupted {
                reason: "user".into(),
            },
            turns: 0,
            usage: Usage::default(),
            stop_reason: houyicoder_protocol::frontend::run::StopReason::EndTurn,
        }),
    };
    app.handle_agent_message(msg);
    assert_eq!(app.input.value(), "draft", "input restored for editing");
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s == "input restored")),
        "restoring abort keeps its input-restored row"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Interrupted)),
        "restoring abort also lands the Interrupted marker"
    );
}

/// Esc1 during a busy run interrupts (queue stays intact); Esc2 recalls the
/// queue head into the input box (merged with any draft). The pop is the
/// user's explicit recall, so the no-content restore on the subsequent
/// Done(Interrupted) -- which re-fills the input with the aborted run's
/// origin - must not clobber it: the queued text was already removed from
/// pending, so an overwrite would lose it entirely. Splitting interrupt
/// from recall stops a panic double-press from wiping the just-recalled
/// message.
#[test]
fn test_esc_pop_survives_interrupt() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.agent_busy = true;
    app.last_run_input = Some("first".into());
    app.handle_agent_message(AgentMessage::Frame(user_msg("first")));
    // Queue a message while the run is in flight (spawn_run's busy path:
    // pending push; no session is wired so the wire side is a no-op).
    app.spawn_run("zzsecond".into());
    assert!(!app.pending.is_empty(), "message queues while busy");
    // Esc1: interrupt the run. The queue stays intact; input stays empty.
    let esc = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    );
    crate::keys::handle_working(&mut app, esc);
    assert!(app.cancelling, "first Esc interrupts the run");
    assert!(!app.pending.is_empty(), "queue intact after the interrupt");
    // Esc2: recall the queue head into the input box.
    crate::keys::handle_working(&mut app, esc);
    assert_eq!(
        app.input.value(),
        "zzsecond",
        "second Esc recalls the queue head"
    );
    // The aborted run settles Interrupted with no real content.
    app.handle_agent_message(AgentMessage::Done {
        result: Ok(RunResult {
            outcome: RunOutcome::Interrupted {
                reason: "user".into(),
            },
            turns: 0,
            usage: Usage::default(),
            stop_reason: houyicoder_protocol::frontend::run::StopReason::EndTurn,
        }),
    });
    assert_eq!(
        app.input.value(),
        "zzsecond",
        "popped text survives the interrupt's input restore"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Interrupted)),
        "interrupt marker still lands"
    );
}

/// The interrupt notice renders as a child row of the message above it, not
/// as a top-level notice: it carries the same gutter prefix tool results use
/// so the reader sees an annotation on that message, not a fresh utterance.
#[test]
fn test_interrupted_renders_child_gutter() {
    let rendered = TranscriptLine::Interrupted.render();
    assert!(
        rendered.starts_with("  ⎿  "),
        "interrupt must use the child gutter, got {rendered:?}"
    );
    assert!(
        !rendered.starts_with('✻'),
        "interrupt must not render as a top-level system notice"
    );
    assert_eq!(rendered, crate::records::INTERRUPTED_NOTICE);
}

#[test]
fn test_max_turns_records_hint() {
    // MaxTurnsReached is a graceful Ok outcome (not an Err payload): the
    // wire carries turns + usage, the TUI surfaces a resume hint.
    let mut app = composition::app();
    let msg = AgentMessage::Done {
        result: Ok(RunResult {
            outcome: RunOutcome::MaxTurnsReached { turns: 5 },
            turns: 5,
            usage: Usage::default(),
            stop_reason: houyicoder_protocol::frontend::run::StopReason::MaxTurnRequests,
        }),
    };
    app.handle_agent_message(msg);
    assert!(!app.agent_busy);
    assert!(app.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::System(s) if s.contains("reached max turns limit")
    )));
}

/// An auto-running tool (requires_approval defaults false) so the drive_loop
/// hits a turn boundary (RunAgain) without an approval popup — for the
/// batch's server-delivery test.
struct EchoTool;
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "auto-run echo for tests"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
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

/// Fast provider, race lost: idle_drain spawns the first message and
/// promotes exactly one parked head, but a fast provider returns before
/// the injected head reaches the server, so the run ends without consuming
/// it. The tail must not be lost -- it stays pending for the next
/// idle_drain. Drives the real wire path (InjectUser -> drive_loop drain
/// -> QueueConsumed -> host remove -> promote next), not just host state.
#[test]
fn test_batch_consumes_via_drain() {
    let provider = Arc::new(FakeProvider::new(vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
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
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let mut app = app_with_provider(provider, tools);
    app.status.last_run_final = true;
    app.pending.push(PendingItem::Message("first".into()));
    app.pending
        .push(PendingItem::ParkedMessage("second".into()));
    app.pending.push(PendingItem::ParkedMessage("third".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(app.agent_busy, "first spawned a run");
    assert_eq!(app.pending.len(), 2, "second/third stay pending");
    assert_eq!(
        app.pending[0],
        PendingItem::Message("second".into()),
        "second holds the copy; third parked behind it"
    );
    assert_eq!(
        app.pending[1],
        PendingItem::ParkedMessage("third".into()),
        "third has no copy (single-copy invariant)"
    );
    let mut tries = 0;
    while app.agent_busy && tries < 1000 {
        app.poll_agent();
        if app.agent_busy {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        tries += 1;
    }
    assert!(!app.agent_busy, "run reached Done");
    let texts: Vec<&str> = app.pending.iter().map(|it| it.display()).collect();
    assert!(
        texts.contains(&"second") && texts.contains(&"third"),
        "tail not lost (race lost, drained next idle_drain):\n{:?}",
        app.pending
    );
}

/// Delayed provider, race won: each InjectUser lands before the next turn
/// boundary, so one run drains the whole queue one boundary at a time and
/// nothing is left pending. The assertion (pending.is_empty()) the
/// race-lost test above cannot make.
#[test]
fn test_batch_delivers_via_drain() {
    let tool_call = |id: &str| CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: id.into(),
            name: "echo".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let done = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider = Arc::new(FakeProvider::new_with_delay(
        vec![tool_call("c1"), tool_call("c2"), done],
        100,
    ));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let mut app = app_with_provider(provider, tools);
    app.status.last_run_final = true;
    app.pending.push(PendingItem::Message("first".into()));
    app.pending
        .push(PendingItem::ParkedMessage("second".into()));
    app.pending.push(PendingItem::ParkedMessage("third".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(app.agent_busy, "first spawned a run");
    assert_eq!(
        app.pending[0],
        PendingItem::Message("second".into()),
        "second holds the copy; third parked"
    );
    let mut tries = 0;
    while app.agent_busy && tries < 1000 {
        app.poll_agent();
        if app.agent_busy {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        tries += 1;
    }
    assert!(!app.agent_busy, "run reached Done");
    assert!(
        app.pending.is_empty(),
        "race won: one run drained the chain one boundary at a time:\n{:?}",
        app.pending
    );
}
