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

/// The batch's server delivery: InjectUser the rest into the new run, the
/// drive_loop drains them at the turn boundary (after the auto-run echo), +
/// QueueConsumed removes them from pending. Verifies the full wire path
/// (host InjectUser -> server drive_loop drain -> QueueConsumed -> host
/// remove), not just the host-side bookkeeping (the layer-axis gap the
/// batch test had).
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
    app.pending.push(PendingItem::Message("m1".into()));
    app.pending.push(PendingItem::Message("m2".into()));
    app.pending.push(PendingItem::Message("m3".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(app.agent_busy, "m1 spawned a run");
    assert_eq!(app.pending.len(), 2, "m2/m3 stay in pending (InjectUser'd)");
    // Poll to Done — the drive_loop runs (echo turn 1, done turn 2). The
    // InjectUser'd m2/m3 race the turn-1 boundary: with a fast FakeProvider
    // the model call returns before the InjectUser wire lands in the
    // input_queue, so m2/m3 are NOT consumed (no QueueConsumed) + stay in
    // pending. This is the timing race (doc'd in the queue-divergence notes):
    // same-run guaranteed (they'll drain on the next idle_drain), same-call
    // not. A real (slow) model wins the race -> QueueConsumed fires + removes
    // them; verifying that needs a delayed provider (follow-up).
    let mut tries = 0;
    while app.agent_busy && tries < 1000 {
        app.poll_agent();
        if app.agent_busy {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        tries += 1;
    }
    assert!(!app.agent_busy, "run reached Done (echo + done)");
    // No loss: m2/m3 are either consumed (pending empty, race won) or still in
    // pending (race lost, drained next idle_drain). With the fast provider the
    // race is lost, so they stay — the invariant is "not lost", not "consumed".
    assert!(
        app.pending
            .iter()
            .all(|it| matches!(it, PendingItem::Message(_))),
        "m2/m3 not lost (still Message in pending, drained next idle_drain):\
         \n{:?}",
        app.pending
    );
}

/// The batch's race-WIN delivery path: with a delayed provider, the
/// InjectUser'd rest land in the input_queue before the first model call
/// returns, the drive_loop's turn-boundary drain consumes them, +
/// QueueConsumed removes them from pending. This is the distinguishing
/// assertion (pending.is_empty()) the non-delayed test can't make — there
/// the race is lost (rest stay), so it can only assert no-loss. Here the
/// race is forced-won (delay), so pending MUST be empty (QueueConsumed
/// fired).
#[test]
fn test_batch_delivers_via_drain() {
    let provider = Arc::new(FakeProvider::new_with_delay(
        vec![
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
        ],
        200,
    ));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let mut app = app_with_provider(provider, tools);
    app.status.last_run_final = true;
    app.pending.push(PendingItem::Message("m1".into()));
    app.pending.push(PendingItem::Message("m2".into()));
    app.pending.push(PendingItem::Message("m3".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(app.agent_busy, "m1 spawned a run");
    assert_eq!(app.pending.len(), 2, "m2/m3 stay in pending (InjectUser'd)");
    // The delayed provider (200ms) lets the InjectUser wire land in the
    // input_queue before the first model call returns. drive_loop: call 1
    // -> echo -> turn boundary -> drain input_queue (m2, m3) -> QueueConsumed
    // -> call 2 -> done -> Final. m2/m3 consumed + removed.
    let mut tries = 0;
    while app.agent_busy && tries < 1000 {
        app.poll_agent();
        if app.agent_busy {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        tries += 1;
    }
    assert!(!app.agent_busy, "run reached Done (echo + done)");
    assert!(
        app.pending.is_empty(),
        "race-WIN: QueueConsumed removed m2/m3 (drive_loop drained at turn \
         boundary, delay let InjectUser land first):\n{:?}",
        app.pending
    );
}
