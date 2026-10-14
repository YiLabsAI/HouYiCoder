//! Streaming behavior for transient output and durable completion frames.
use super::*;
use crate::agent_message::ServerResponse;
use crate::input::queue::InputItem;
use crate::keys::handle_working;
use crate::records::INTERRUPTED_NOTICE;
use crate::row_id::RowId;
use crate::state::Screen;
use crate::test_harness::attach_connection;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::QueuedInput;
use houyicoder_protocol::frontend::run::StopReason;
use std::thread::sleep;
use std::time::Duration;

/// Streamed reasoning remains transient until completion records it in the
/// transcript.
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
        if !app.agent_busy() {
            got = true;
            break;
        }
        sleep(Duration::from_millis(10));
    }
    assert!(got, "run should settle");
    // The durable Reasoning event lands as a first-class Thinking line.
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Thinking { text } if text.contains("pondering"))),
        "durable Thinking line missing: {:?}",
        app.sessions.current.transcript
    );
    // And the assistant text follows it.
    assert!(app.sessions.current.transcript.iter().any(|l| matches!(
        l,
        TranscriptLine::Agent(s) if s.contains("answer")
    )));
    // Live preview cleared after Done.
    assert!(
        app.run_progress()
            .is_none_or(|p| p.live_reasoning_text.is_empty())
    );
}

/// A turn's summary row is derived from the durable frames, not pushed from
/// the live run: the record that closes the turn lands in the frame log, and
/// the rebuild folds the turn's reasoning into its row there. Driving the real
/// run end to end is what proves the delivery order the derivation needs — the
/// record reaches the frontend, and the rebuild that reads it covers the turn
/// the record closes rather than only the frames after it.
#[test]
fn test_run_row_from_record() {
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
    let mut settled = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy() {
            settled = true;
            break;
        }
        sleep(Duration::from_millis(10));
    }
    assert!(settled, "run should settle within the poll window");
    let row = app
        .sessions
        .current
        .transcript
        .iter()
        .find_map(|l| match l {
            TranscriptLine::ThoughtFor {
                reasoning, turn_id, ..
            } => Some((reasoning.clone(), turn_id.clone())),
            _ => None,
        });
    let (reasoning, turn_id) = row.expect("the finished turn must surface its summary row");
    assert_eq!(reasoning.as_deref(), Some("pondering the task"));
    assert!(
        matches!(turn_id, RowId::Durable { slot: 1, .. }),
        "the record that closed the turn names the row by its durable \
         identity, at the notification slot: {turn_id:?}"
    );
}

/// Regression guard for output-tail truncation. A streamed reply must land in
/// the transcript in full after Done — head and the last 4-char delta (the
/// tail). The response stream handler ships each chunk via try_send on a
/// bounded channel; the authoritative AssistantMessage frame replaces that
/// preview, so the rebuild from frames must carry every token. A
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
        if !app.agent_busy() {
            settled = true;
            break;
        }
        sleep(Duration::from_millis(10));
    }
    assert!(settled, "run should settle within the poll window");
    // The authoritative AssistantMessage frame must drive one Agent line whose
    // text is the full streamed reply. Concatenate every Agent line so a split
    // across lines (e.g. a length-recovery continuation) still passes.
    let agent_text = app
        .sessions
        .current
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
        app.run_progress()
            .is_none_or(|p| p.live_assistant_text.is_empty()),
        "live preview should be cleared after Done"
    );
}

/// The durable assistant frame clears its live preview immediately, before
/// the run ends, so one paragraph cannot render both at its event position and
/// again at the live transcript tail while a following tool remains active.
#[test]
fn test_durable_clears_preview() {
    let mut app = composition::app();
    app.start_run_for_test(0);
    {
        let p = app.run_progress_mut().expect("active run");
        p.live_active = true;
        p.live_assistant_text = "same paragraph".into();
    }

    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        agent_msg("same paragraph").into(),
    )));

    let p = app.run_progress().expect("active run");
    assert!(p.live_assistant_text.is_empty());
    assert!(!p.live_active);
    assert_eq!(
        app.sessions
            .current
            .transcript
            .iter()
            .filter(|line| matches!(line, TranscriptLine::Agent(text) if text == "same paragraph"))
            .count(),
        1
    );
}

/// A durable response to a mid-turn user frame stays directly after that user
/// frame instead of also appearing at the live tail.
#[test]
fn test_midturn_keeps_order() {
    let mut app = composition::app();
    app.start_run_for_test(0);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("original").into(),
    )));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        agent_msg("earlier work").into(),
    )));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("interjection").into(),
    )));
    {
        let p = app.run_progress_mut().expect("active run");
        p.live_active = true;
        p.live_assistant_text = "response".into();
    }

    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        agent_msg("response").into(),
    )));

    let user = app
        .sessions
        .current
        .transcript
        .iter()
        .position(|line| matches!(line, TranscriptLine::User(text) if text == "interjection"))
        .expect("interjection line");
    let responses: Vec<_> = app
        .sessions
        .current
        .transcript
        .iter()
        .enumerate()
        .filter(|(_, line)| matches!(line, TranscriptLine::Agent(text) if text == "response"))
        .collect();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].0, user + 1);
    assert!(
        app.run_progress()
            .expect("active run")
            .live_assistant_text
            .is_empty()
    );
}

/// Esc aborting a run that already produced real content (agent text landed)
/// must surface a visible Interrupted row — the silent-Esc bug left no trace
/// in the transcript. A dim one-line interrupt marker.
#[test]
fn test_interrupted_content_shows_marker() {
    let mut app = composition::app();
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("hi").into(),
    )));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        agent_msg("partial reply").into(),
    )));
    let msg = SessionMessage::Response {
        request: RequestId(11),
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::Interrupted {
                    reason: "user".into(),
                },
                turns: 1,
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            }),
        },
    };
    app.start_run_for_test(11);
    app.handle_agent_message(msg);
    assert!(!app.agent_busy(), "Interrupted clears busy");
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Interrupted)),
        "Interrupted lands a visible marker row even with real content"
    );
}

/// Esc aborting a run that produced no real content restores the input and
/// surfaces both the input-restored row and the Interrupted marker.
#[test]
fn test_interrupt_restores_input() {
    let mut app = composition::app();
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("draft").into(),
    )));
    app.sessions.current.last_run_input = Some("draft".into());
    app.ui
        .input
        .queue
        .push(InputItem::ParkedMessage("queued".into()));
    let msg = SessionMessage::Response {
        request: RequestId(12),
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::Interrupted {
                    reason: "user".into(),
                },
                turns: 0,
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            }),
        },
    };
    app.start_run_for_test(12);
    app.handle_agent_message(msg);
    assert_eq!(
        app.ui.input.editor.value(),
        "draft",
        "input restored for editing"
    );
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s == "input restored")),
        "restoring abort keeps its input-restored row"
    );
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Interrupted)),
        "restoring abort also lands the Interrupted marker"
    );
    handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.ui.input.editor.value(),
        "draft",
        "a repeated Esc keeps the restore"
    );
    assert_eq!(
        app.ui.input.queue.len(),
        1,
        "Esc does not recall pending input"
    );
}

/// A committed mid-turn input closes the original turn's rollback window.
/// Interruption must preserve the committed user line rather than restoring
/// the stale run origin.
#[test]
fn test_commit_blocks_restore() {
    let mut app = composition::app();
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("origin").into(),
    )));
    app.sessions.current.last_run_input = Some("origin".into());
    let committed = QueuedInput::new("follow-up");
    app.ui
        .input
        .queue
        .push(InputItem::Message(committed.clone()));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("follow-up").into(),
    )));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::QueuedInputCommitted {
        inputs: vec![committed],
    }));

    app.start_run_for_test(13);
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(13),
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::Interrupted {
                    reason: "user".into(),
                },
                turns: 0,
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            }),
        },
    });

    assert!(
        app.ui.input.editor.is_empty(),
        "stale origin is not restored"
    );
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|line| matches!(line, TranscriptLine::User(text) if text == "follow-up"))
    );
}

/// Visible live output closes the rollback window even if interruption wins
/// before the matching durable assistant frame reaches the frontend.
#[test]
fn test_live_blocks_restore() {
    let mut app = composition::app();
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("origin").into(),
    )));
    app.sessions.current.last_run_input = Some("origin".into());
    app.start_run_for_test(14);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Delta {
        text: "visible response".into(),
    }));

    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(14),
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::Interrupted {
                    reason: "user".into(),
                },
                turns: 0,
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            }),
        },
    });

    assert!(
        app.ui.input.editor.is_empty(),
        "visible output prevents rollback"
    );
    assert!(
        app.sessions.current.last_run_input.is_none(),
        "rollback stash is cleared"
    );
}

/// Esc during a busy run interrupts while an explicit queue action recalls
/// the queued input. The no-content restore on the subsequent interruption
/// must not clobber the recalled text because the queued item was removed from
/// pending, so an overwrite would lose it entirely. Splitting interrupt
/// from recall stops a panic double-press from wiping the just-recalled
/// message.
#[test]
fn test_recall_survives_interrupt() {
    let mut app = composition::app();
    attach_connection(&mut app);
    app.ui.navigation.screen = Screen::Working;
    app.start_run_for_test(0);
    app.sessions.current.last_run_input = Some("first".into());
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("first").into(),
    )));
    // Queue a message while the run is in flight (spawn_run's busy path:
    // pending push; the busy branch takes no send).
    app.spawn_run("zzsecond".into());
    assert!(!app.ui.input.queue.is_empty(), "message queues while busy");
    // Esc1: interrupt the run. The queue stays intact; input stays empty.
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_working(&mut app, esc);
    assert!(app.cancelling(), "first Esc interrupts the run");
    assert!(
        !app.ui.input.queue.is_empty(),
        "queue intact after the interrupt"
    );
    app.pop_queued_to_input();
    assert_eq!(
        app.ui.input.editor.value(),
        "zzsecond",
        "explicit recall moves the queue head into the input"
    );
    // The aborted run settles Interrupted with no real content.
    app.start_run_for_test(15);
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(15),
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::Interrupted {
                    reason: "user".into(),
                },
                turns: 0,
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            }),
        },
    });
    assert_eq!(
        app.ui.input.editor.value(),
        "zzsecond",
        "popped text survives the interrupt's input restore"
    );
    assert!(
        app.sessions
            .current
            .transcript
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
    assert_eq!(rendered, INTERRUPTED_NOTICE);
}

/// A second interrupted turn must not stack a second Interrupted row: the
/// resubmit of a restored input strips the prior restore pair, so each
/// interrupt leaves exactly one marker and one restore notice.
#[test]
fn test_resubmit_avoids_duplicate_interrupt() {
    let interrupted_done = |req: RequestId| SessionMessage::Response {
        request: req,
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::Interrupted {
                    reason: "user".into(),
                },
                turns: 0,
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            }),
        },
    };
    let interrupts = |app: &App| {
        app.sessions
            .current
            .transcript
            .iter()
            .filter(|l| matches!(l, TranscriptLine::Interrupted))
            .count()
    };
    let restore_notices = |app: &App| {
        app.sessions
            .current
            .transcript
            .iter()
            .filter(|l| matches!(l, TranscriptLine::System(s) if s == "input restored"))
            .count()
    };

    let mut app = composition::app();
    attach_connection(&mut app);
    app.ui.navigation.screen = Screen::Working;

    // Turn 1: submit, then interrupt before output -> restore for editing.
    app.spawn_run("analysis".into());
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("analysis").into(),
    )));
    let turn1 = app.active_run_req_id().expect("turn 1 running");
    app.handle_agent_message(interrupted_done(turn1));
    assert_eq!(app.ui.input.editor.value(), "analysis", "turn 1 restored");
    assert_eq!(interrupts(&app), 1, "one Interrupted after turn 1");

    // Resubmit the restored input, then interrupt again.
    drop(app.ui.input.editor.take());
    app.spawn_run("analysis".into());
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("analysis").into(),
    )));
    let turn2 = app.active_run_req_id().expect("turn 2 running");
    app.handle_agent_message(interrupted_done(turn2));

    assert_eq!(
        interrupts(&app),
        1,
        "the resubmit stripped the prior pair; one Interrupted remains: {:?}",
        app.sessions.current.transcript
    );
    assert_eq!(
        restore_notices(&app),
        1,
        "only the new restore notice remains: {:?}",
        app.sessions.current.transcript
    );
}

#[test]
fn test_max_turns_records_hint() {
    // MaxTurnsReached is a graceful Ok outcome (not an Err payload): the
    // wire carries turns + usage, the TUI surfaces a resume hint.
    let mut app = composition::app();
    let msg = SessionMessage::Response {
        request: RequestId(16),
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::MaxTurnsReached { turns: 5 },
                turns: 5,
                usage: Usage::default(),
                stop_reason: StopReason::MaxTurnRequests,
            }),
        },
    };
    app.start_run_for_test(16);
    app.handle_agent_message(msg);
    assert!(!app.agent_busy());
    assert!(app.sessions.current.transcript.iter().any(|l| matches!(
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
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move { Ok(input) })
    }
}

/// A fast provider races the first promoted injection. The server may consume
/// the second item at its tool boundary or finish before it arrives, but the
/// third item has no later boundary and must remain pending either way.
#[test]
fn test_batch_keeps_tail() {
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
    app.sessions.current.last_run_final = true;
    app.ui.input.queue.push(InputItem::Message("first".into()));
    app.ui
        .input
        .queue
        .push(InputItem::ParkedMessage("second".into()));
    app.ui
        .input
        .queue
        .push(InputItem::ParkedMessage("third".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(app.agent_busy(), "first spawned a run");
    assert_eq!(app.ui.input.queue.len(), 2, "second/third stay pending");
    assert_eq!(
        app.ui.input.queue[0],
        InputItem::Message("second".into()),
        "second holds the copy; third parked behind it"
    );
    assert_eq!(
        app.ui.input.queue[1],
        InputItem::ParkedMessage("third".into()),
        "third has no copy (single-copy invariant)"
    );
    let mut tries = 0;
    while app.agent_busy() && tries < 1000 {
        app.poll_agent();
        if app.agent_busy() {
            sleep(Duration::from_millis(5));
        }
        tries += 1;
    }
    assert!(!app.agent_busy(), "run reached Done");
    let texts: Vec<&str> = app.ui.input.queue.iter().map(|it| it.display()).collect();
    assert!(
        matches!(texts.as_slice(), ["second", "third"] | ["third"]),
        "the undrainable tail must remain in FIFO order: {:?}",
        app.ui.input.queue
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
    app.sessions.current.last_run_final = true;
    app.ui.input.queue.push(InputItem::Message("first".into()));
    app.ui
        .input
        .queue
        .push(InputItem::ParkedMessage("second".into()));
    app.ui
        .input
        .queue
        .push(InputItem::ParkedMessage("third".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(app.agent_busy(), "first spawned a run");
    assert_eq!(
        app.ui.input.queue[0],
        InputItem::Message("second".into()),
        "second holds the copy; third parked"
    );
    let mut tries = 0;
    while app.agent_busy() && tries < 1000 {
        app.poll_agent();
        if app.agent_busy() {
            sleep(Duration::from_millis(5));
        }
        tries += 1;
    }
    assert!(!app.agent_busy(), "run reached Done");
    assert!(
        app.ui.input.queue.is_empty(),
        "race won: one run drained the chain one boundary at a time:\n{:?}",
        app.ui.input.queue
    );
}

/// A lone Interrupted notice is cleared by the next submission. A turn with
/// real output gets no restore line, so the notice cannot be dropped as a
/// trailing pair: left in place it survives every rebuild and reads as a fresh
/// interruption at each later run's start.
#[test]
fn test_submit_clears_lone_interrupt() {
    let mut app = composition::app();
    attach_connection(&mut app);
    app.ui.navigation.screen = Screen::Working;
    // A run with real output, interrupted: a lone notice, no restore pair.
    app.spawn_run("first".into());
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        user_msg("first").into(),
    )));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        agent_msg("partial reply").into(),
    )));
    let req = app.active_run_req_id().expect("run active");
    app.handle_agent_message(SessionMessage::Response {
        request: req,
        response: ServerResponse::Done {
            result: Ok(RunResult {
                outcome: RunOutcome::Interrupted {
                    reason: "user".into(),
                },
                turns: 1,
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            }),
        },
    });
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Interrupted)),
        "the interrupted run lands its notice"
    );
    assert!(
        !app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s == "input restored")),
        "real output means no restore line, so the notice stands alone"
    );
    // The next submission answers the notice.
    app.spawn_run("second".into());
    assert!(
        !app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Interrupted)),
        "an answered notice must not ride into the next turn: {:?}",
        app.sessions.current.transcript
    );
}
