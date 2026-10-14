use super::*;
use crate::agent_message::ConnectionEvent;
use crate::fold::DisplaySlot;
use crate::input::queue::InputItem;
use crate::input::queue::resume_command_target;
use crate::pending_prompt::PendingPrompt;
use crate::records::ToolOutcome;
use crate::records::TranscriptLine;
use crate::row_id::RowId;
use crate::state::history_read::HistoryReadOutcome;
use crate::state::transcript::DiskFront;
use crate::transcript::{SequencedFrame, TranscriptFrame};
use houyicoder_protocol::envelope::RequestId;
use std::sync::atomic::Ordering;

/// The resolved target carried by a queued /resume Command at the head.
fn queued_target(app: &App) -> Option<String> {
    app.ui
        .input
        .queue
        .first()
        .and_then(resume_command_target)
        .map(str::to_string)
}

/// Enqueue a /resume Command with a resolved target at the head.
fn enqueue_resume_target(app: &mut App, target: &str) {
    app.ui
        .input
        .queue
        .insert(0, InputItem::Command(format!("/resume {target}")));
}

fn test_bundle() -> (RunnerBundle, FrontendStartup) {
    let bundle = houyicoder_service::composition::build_runner(
        houyicoder_service::composition::BuildRunnerOptions::default(),
    );
    let runner = bundle.runner;
    let session = bundle.session;
    let gate = bundle.gate;
    let append_notify = bundle.append_notify;
    let wire_session = houyicoder_protocol::frontend::SessionId(session.to_string());
    let (tx, rx) = mpsc::channel::<SessionMessage>();
    let (runner, client, startup_warnings) =
        start_local_server(runner, session, gate, append_notify, None);
    drop(runner);
    (
        RunnerBundle {
            client,
            agent_tx: tx,
            agent_rx: rx,
            session: wire_session,
            model: "test-model".to_string(),
            trajectory_log: None,
            export_log: None,
            snapshot: None,
            session_catalog: None,
        },
        FrontendStartup {
            skip_login: false,
            startup_warnings,
            history_path: std::env::temp_dir().join("houyi-history-swap-test.jsonl"),
        },
    )
}

/// Build a working App from the test pairing, applying both the session
/// bundle and the frontend startup policy the pairing resolved. Mirrors the
/// production composition root, which never builds a Session without also
/// resolving the frontend policy.
fn test_app() -> App {
    let (bundle, startup) = test_bundle();
    build_app(bundle, startup)
}

/// Like test_bundle but returns the server task's JoinHandle so a test can
/// assert the serve loop exited after the session is torn down.
fn test_bundle_tracked() -> ((RunnerBundle, FrontendStartup), tokio::task::JoinHandle<()>) {
    let bundle = houyicoder_service::composition::build_runner(
        houyicoder_service::composition::BuildRunnerOptions::default(),
    );
    let runner = bundle.runner;
    let session = bundle.session;
    let gate = bundle.gate;
    let append_notify = bundle.append_notify;
    let wire_session = houyicoder_protocol::frontend::SessionId(session.to_string());
    let (tx, rx) = mpsc::channel::<SessionMessage>();
    let (runner, client, serve, startup_warnings) =
        start_local_server_tracked(runner, session, gate, append_notify, None);
    drop(runner);
    (
        (
            RunnerBundle {
                client,
                agent_tx: tx,
                agent_rx: rx,
                session: wire_session,
                model: "test-model".to_string(),
                trajectory_log: None,
                export_log: None,
                snapshot: None,
                session_catalog: None,
            },
            FrontendStartup {
                skip_login: false,
                startup_warnings,
                history_path: std::env::temp_dir().join("houyi-history-swap2-test.jsonl"),
            },
        ),
        serve,
    )
}

#[test]
fn test_switch_session_resets_view() {
    let mut app = test_app();
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::User("old content".into()));
    app.sessions
        .current
        .transcript
        .push_frame(TranscriptFrame::Session(
            houyicoder_protocol::frontend::session_update::SessionUpdate::UserMessageChunk(
                houyicoder_protocol::frontend::session_update::ContentChunk::new(
                    houyicoder_protocol::frontend::run::ContentBlock::Text {
                        text: "old frame".into(),
                    },
                ),
            ),
        ));
    let (new_bundle, new_startup) = test_bundle();
    let new_sid = new_bundle.session.0.clone();
    let new_warning_count = new_startup.startup_warnings.len();
    app.switch_session(new_bundle);
    app.apply_frontend_startup(new_startup);
    // swap clears the old transcript; the new bundle's startup warnings
    // (e.g. a sandbox-fence notice) land as initial system lines, so the
    // transcript has exactly that many lines, not the old content.
    assert_eq!(
        app.sessions.current.transcript.len(),
        new_warning_count,
        "transcript cleared of old content, only new startup warnings remain"
    );
    assert!(
        !app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("old content"))),
        "old content gone"
    );
    // The new session's startup warnings enter the frame log as frontend
    // rows, so the log comes out empty only on a host where the runner has
    // nothing to warn about. The invariant that holds on any host is that
    // the turn stream the previous session received from the server does
    // not survive the switch.
    assert!(
        !app.sessions
            .current
            .transcript
            .frames()
            .iter()
            .any(|f| matches!(
                f,
                SequencedFrame {
                    frame: TranscriptFrame::Session(_),
                    ..
                }
            )),
        "previous session frames cleared"
    );
    assert_eq!(
        app.sessions.current.identity.session_id.0, new_sid,
        "session_id updated"
    );
    assert!(
        app.sessions.current.connection.is_some(),
        "new session driver wired"
    );
    assert!(!app.agent_busy(), "agent_busy cleared");
    assert!(app.whole_scroll().is_following_tail(), "scroll reset");
    assert_eq!(
        app.ui.navigation.pane,
        crate::state::Pane::Transcript,
        "pane reset"
    );
}

/// A pending history read was dispatched against the old session's log. The
/// switch rebuilds the whole App, so the record goes with the old session and
/// the new owner counts nothing: the abandoned worker's slot belonged to the
/// counter it was dispatched against, not to the one the new session reads. The
/// old worker's send fails into the dropped receiver, and the drop told it to
/// stop, so it quits at its next window rather than walking an archived log to
/// its byte cap. A late result from the archived session has no route into the
/// new view.
#[test]
fn test_switch_drops_pending_read() {
    let mut app = test_app();
    let (tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let lease = app
        .sessions
        .current
        .history_reads
        .dispatch(0, DiskFront::Unloaded, rx);
    assert_eq!(app.sessions.current.history_reads.running(), 1);

    app.switch_session(test_bundle().0);

    assert!(
        !app.sessions.current.history_reads.holds_pending(),
        "the rebuilt App starts with no read against the old log"
    );
    assert_eq!(
        app.sessions.current.history_reads.running(),
        0,
        "and it does not inherit the slot the abandoned worker still holds"
    );
    assert!(
        lease.stop().load(Ordering::Relaxed),
        "the abandoned worker was told to stop, not merely unheard"
    );
    assert!(
        tx.send(HistoryReadOutcome::Exhausted).is_err(),
        "the old worker's route to the view is gone"
    );
}

#[test]
fn test_swap_bumps_transcript_version() {
    let mut app = test_app();
    let v_before = app.sessions.current.transcript.revision();
    app.switch_session(test_bundle().0);
    assert_ne!(
        app.sessions.current.transcript.revision(),
        v_before,
        "version bumped to invalidate cache"
    );
}

/// build_app pushes the bundle's startup_warnings as initial transcript
/// system lines (synchronous, before any run output). A bad settings value
/// surfaces as a system line instead of silently dropping.
#[test]
fn test_build_app_surfaces_warnings() {
    let (bundle, mut startup) = test_bundle();
    startup
        .startup_warnings
        .push("model.effort_level: bad".into());
    startup
        .startup_warnings
        .push("sandbox.network: unknown".into());
    let app = build_app(bundle, startup);
    let lines: Vec<&str> = app
        .sessions
        .current
        .transcript
        .iter()
        .rev()
        .filter_map(|l| match l {
            TranscriptLine::System(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    // The two warnings land as system lines (in queue order).
    assert!(
        lines.iter().any(|s| s.contains("model.effort_level: bad")),
        "first startup warning surfaced: {lines:?}"
    );
    assert!(
        lines.iter().any(|s| s.contains("sandbox.network: unknown")),
        "second startup warning surfaced: {lines:?}"
    );
}

#[test]
fn test_try_noop_without_target() {
    let mut app = test_app();
    let mut dirty = false;
    app.try_switch_session(None, &mut dirty);
    assert!(!dirty, "no target = no-op");
    assert!(!app.ui.quit, "no target = no-op");
}

/// The old busy put-back branch is gone: the event loop's idle guard
/// (!agent_busy && !reverse_request_in_flight) now gates try_switch_session,
/// so it only runs when idle. A test that set agent_busy=true + called
/// try_switch_session directly would exercise a path the caller no longer
/// reaches; the idle-guarded swap is covered by try_swap_swaps_when_idle
/// + the PTY journeys (resume_sid_reopens_history, resume_picker_swaps).

#[test]
fn test_try_no_builder_quits() {
    let mut app = test_app();
    enqueue_resume_target(&mut app, "some-sid");
    let mut dirty = false;
    app.try_switch_session(None, &mut dirty);
    assert!(!dirty, "no builder = no swap");
    assert!(app.ui.quit, "no builder = fallback quit");
    assert_eq!(
        queued_target(&app).as_deref(),
        Some("some-sid"),
        "target put back for caller"
    );
}

/// The no-builder path through idle_drain with a clean run end: the quit
/// guard keeps the /resume on the queue so the loop-exit drain hands the
/// target up. Without the guard, drain_pending_head would remove +
/// re-dispatch the command, relying on run_resume re-enqueuing it.
#[test]
fn test_quit_keeps_resume_target() {
    let mut app = test_app();
    enqueue_resume_target(&mut app, "some-sid");
    app.sessions.current.last_run_final = true;
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(!dirty, "no swap = no dirty");
    assert!(app.ui.quit, "no builder sets quit");
    assert_eq!(
        queued_target(&app).as_deref(),
        Some("some-sid"),
        "the target stays queued for the loop-exit drain"
    );
}

/// A resume target naming the session already presented is a no-op: the
/// live lease holds that sid's lock, so re-acquiring would report it held
/// by this process itself. The fast path reports cleanly without calling
/// the builder, so no swap and no quit.
#[test]
fn test_same_session_noop() {
    let mut app = test_app();
    let current_sid = app.sessions.current.identity.session_id.0.clone();
    enqueue_resume_target(&mut app, &current_sid);
    let mut dirty = false;
    let builder: Box<ResumeBuilderRef> =
        Box::new(|_| panic!("builder must not run for a same-session no-op"));
    app.try_switch_session(Some(&*builder), &mut dirty);
    assert!(dirty, "the no-op system line flags dirty");
    assert!(!app.ui.quit, "same-session is a no-op, not a quit");
    assert!(
        queued_target(&app).is_none(),
        "target consumed on no-op (no retry-every-frame storm)"
    );
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("already on session"))),
        "an 'already on session' system line must surface"
    );
}

/// Adversarial: a resume_builder that returns Err surfaces a system line +
/// does NOT swap (the session stays), does NOT quit (unlike the no-builder
/// fallback). A builder failure (disk read, sid resolution, provider) must
/// not crash or silently no-op — the user is told, the current session is
/// preserved, and the target is consumed (not retried every frame).
#[test]
fn test_resume_err_keeps_session() {
    let mut app = test_app();
    enqueue_resume_target(&mut app, "bad-sid");
    let mut dirty = false;
    let builder: Box<ResumeBuilderRef> = Box::new(|_| Err("sid not on disk".into()));
    app.try_switch_session(Some(&*builder), &mut dirty);
    assert!(dirty, "the error system line flags dirty");
    assert!(
        !app.ui.quit,
        "a builder error is not a quit (unlike no builder)"
    );
    assert!(
        queued_target(&app).is_none(),
        "target consumed on error (no retry-every-frame storm)"
    );
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("resume failed"))),
        "a 'resume failed' system line must surface the error"
    );
}

/// idle_drain is the event loop's convergence point: continuous-state
/// polling + consumptive idempotency. Idle + a clean run end (FinalOutput) +
/// a queued message drains the head; busy, a pending reverse request, or a
/// non-clean end (interrupt/error) is a no-op (the queued item parks for
/// the user to recall + edit).
#[test]
fn test_idle_drain_consumes_idle() {
    let mut app = test_app();
    app.sessions.current.last_run_final = true;
    app.ui.input.queue.push(InputItem::Message("head".into()));
    let mut dirty = false;
    // No resume target + idle + clean end: drain the queued head.
    app.idle_drain(None, &mut dirty);
    assert!(dirty, "drain flagged dirty");
    assert!(app.ui.input.queue.is_empty(), "head drained");
}

/// Busy (a run in flight): idle_drain is a no-op -- neither swap nor
/// drain. The continuous-state poll runs every frame but consumes nothing.
#[test]
fn test_idle_drain_noop_busy() {
    let mut app = test_app();
    app.start_run_for_test(0);
    app.ui.input.queue.push(InputItem::Message("queued".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(!dirty, "busy = no drain");
    assert_eq!(
        app.ui.input.queue,
        vec![InputItem::Message("queued".into())],
        "queue intact"
    );
}

/// A non-clean run end (interrupt / error) does NOT auto-drain: the queued
/// item parks for the user to pop to the input box via Esc + edit before
/// re-sending. A redirect on interrupt should not auto-fire the pending
/// input. A clean end (FinalOutput) auto-sends; see
/// idle_drain_consumes_when_idle.
#[test]
fn test_idle_drain_without_final() {
    let mut app = test_app();
    app.sessions.current.last_run_final = false;
    app.ui
        .input
        .queue
        .push(InputItem::ParkedMessage("parked".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(!dirty, "no drain on a non-clean end (interrupt/error)");
    assert_eq!(
        app.ui.input.queue,
        vec![InputItem::ParkedMessage("parked".into())],
        "queued item parks (not auto-sent) -- recallable via Esc"
    );
}

/// A clean run end (FinalOutput) drains a parked head: the queued item
/// auto-sends as the next turn. The parked path (no server copy) spawns a
/// fresh run; this is the drain_pending_head ParkedMessage branch under a
/// wired session.
#[test]
fn test_clean_end_drains_parked() {
    let mut app = test_app();
    app.sessions.current.last_run_final = true;
    app.ui
        .input
        .queue
        .push(InputItem::ParkedMessage("parked".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(dirty, "drain on a clean end (FinalOutput)");
    assert!(
        app.ui.input.queue.is_empty(),
        "parked head drained (spawn_run)"
    );
}

/// A deferred resume target with a queued message: idle_drain acts on the
/// resume target first (try_switch_session runs before drain_pending_head in
/// idle_drain), so the swap happens before the message drains. The swap
/// carries the pending queue across (demoting Messages to ParkedMessage --
/// the new runner's server queue is empty), then the head drains in the new
/// session, auto-sending the carried message as the first turn in B.
#[test]
fn test_resume_precedes_queued_message() {
    let mut app = test_app();
    enqueue_resume_target(&mut app, "sid-target");
    app.ui
        .input
        .queue
        .push(InputItem::Message("queued msg".into()));
    let mut dirty = false;
    let builder: Box<ResumeBuilderRef> = Box::new(|_| Ok(test_bundle()));
    app.idle_drain(Some(&*builder), &mut dirty);
    assert!(dirty, "swap flagged dirty");
    assert!(
        queued_target(&app).is_none(),
        "resume target consumed before the message drains"
    );
    assert!(
        app.ui.input.queue.is_empty(),
        "carried message drained in the new session (auto-sent as B's first turn)"
    );
}

/// switch_session must clear session-local state: the old session's todo
/// list, token/step counts, pending approval, text selection, and the
/// row caches for selection/copy. None of those survive a swap -- the
/// new session must render as if it were fresh, not layered over the old
/// one's todos/tokens/selection pointing at stale rows. The pending queue
/// is NOT in this set: it is carried across (see swap_carries_pending).
#[test]
fn test_clears_session_local_state() {
    use crate::todo_view::{TodoStatus, TodoView};
    let mut app = test_app();
    // Populate with old-session residue.
    app.sessions.current.todos.items.push(TodoView {
        content: "old todo".into(),
        status: TodoStatus::InProgress,
        active_form: None,
    });
    app.sessions.current.run_metrics.cumulative_tokens = 1234;
    app.sessions.current.run_metrics.cumulative_steps = 7;
    app.ui.displayed_tokens.set(50);
    app.sessions.current.prompt = Some(PendingPrompt::approval_card(
        RequestId(0),
        crate::records::Approval::default(),
    ));
    app.ui.selection.transcript.anchor = Some((0, 5));
    app.ui.selection.transcript.is_dragging = true;
    app.ui
        .frame
        .layout
        .last_all_rows
        .borrow_mut()
        .push((0, "stale row".into()));
    app.ui
        .frame
        .keys
        .borrow_mut()
        .push_callid_for_test(Some("stale-call".into()));
    // Sanity: the residue is there before the swap.
    assert!(!app.sessions.current.todos.items.is_empty());
    assert!(app.ui.selection.transcript.anchor.is_some());

    app.switch_session(test_bundle().0);

    assert!(app.sessions.current.todos.items.is_empty(), "todos cleared");
    assert_eq!(
        app.sessions.current.run_metrics.cumulative_tokens, 0,
        "cumulative_tokens cleared"
    );
    assert_eq!(
        app.sessions.current.run_metrics.cumulative_steps, 0,
        "cumulative_steps cleared"
    );
    assert_eq!(app.ui.displayed_tokens.get(), 0, "displayed_tokens cleared");
    assert!(app.approval().is_none(), "approval cleared");
    assert!(
        app.ui.selection.transcript.anchor.is_none(),
        "selection anchor cleared"
    );
    assert!(
        !app.ui.selection.transcript.is_dragging,
        "selection drag cleared"
    );
    assert!(
        app.ui.frame.layout.last_all_rows.borrow().is_empty(),
        "last_all_rows cleared"
    );
    assert!(
        app.ui.frame.keys.borrow().is_empty(),
        "visible call ids cleared"
    );
}

/// switch_session carries the pending queue across the swap, demoting Messages
/// to ParkedMessage (the new runner's server queue is empty, so a live server copy
/// is lost). A command ahead of messages stays ahead; the queue order is
/// preserved. The carried items auto-drain in the new session on the next
/// idle (a queued message from A sends in B as the next turn), unless a Command barrier holds.
#[test]
fn test_swap_carries_pending_across() {
    let mut app = test_app();
    app.ui.input.queue.push(InputItem::Message("task a".into()));
    app.ui
        .input
        .queue
        .push(InputItem::Command("/resume sid-b".into()));
    app.ui.input.queue.push(InputItem::Message("task c".into()));

    app.switch_session(test_bundle().0);

    assert_eq!(
        app.ui.input.queue,
        vec![
            InputItem::ParkedMessage("task a".into()),
            InputItem::Command("/resume sid-b".into()),
            InputItem::ParkedMessage("task c".into()),
        ],
        "queue carried across, order preserved; messages demoted to \
         ParkedMessage (new runner's server queue is empty)"
    );
}

/// Teardown: a swap drops the old session, which drops the old driver's
/// command channel, so the old driver exits, which drops the old client,
/// which drops the wire channel, so the old server's serve loop sees a
/// clean disconnect and exits. No orphan server task lingers. This is the
/// foundation that makes abandoning any InjectUser'd queue items safe (the
/// abandoned items die with the old server). Polled (not async-awaited)
/// because the shared runtime runs the server task on its own threads.
#[test]
fn test_tears_down_old_server() {
    let ((bundle, bundle_startup), old_serve) = test_bundle_tracked();
    let mut app = build_app(bundle, bundle_startup);
    // The old server serves an established session before the swap: wait for
    // the driver's readiness announcement, which the handshake produces once,
    // so the server is proven to be serving rather than merely not yet dead.
    let msg = app
        .sessions
        .current
        .connection
        .as_mut()
        .expect("session")
        .poll_startup(std::time::Duration::from_secs(5))
        .expect("the handshake result arrives");
    assert!(
        matches!(msg, SessionMessage::Connection(ConnectionEvent::Ready)),
        "the driver announces readiness, got {msg:?}"
    );
    app.handle_agent_message(msg);
    assert!(
        !old_serve.is_finished(),
        "old server alive before swap (handshake done, waiting on input)"
    );
    // Swap: build_app resets self, dropping the old connection. The driver
    // JoinHandle is detached (the task is not aborted), but the driver exits
    // when its command channel returns None.
    app.switch_session(test_bundle().0);
    // The teardown chain is async on the shared runtime; poll for completion.
    let mut tries = 0;
    while !old_serve.is_finished() && tries < 200 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        tries += 1;
    }
    assert!(
        old_serve.is_finished(),
        "old server serve loop must exit after the swap drops the session \
         (command channel drop -> driver exit -> client drop -> server \
         disconnect). Polled {tries} times (2s); still running -- orphan \
         server leak."
    );
}

/// Adversarial: a pending approval (reverse_request_in_flight) blocks
/// idle_drain entirely -- no swap, no message drain -- so a /resume issued
/// while a permission card is up does not yank the session out from under
/// the approval. The approval resolves first; the deferred swap runs at the
/// next idle. Matches the busy guard (idle_drain_noop_when_busy) for the
/// approval-pending axis the busy test does not cover.
#[test]
fn test_idle_drain_noop_approval() {
    let mut app = test_app();
    app.sessions.current.prompt = Some(PendingPrompt::permission(RequestId(42), Vec::new()));
    app.ui.input.queue.push(InputItem::Message("queued".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(!dirty, "approval pending = no drain and no swap");
    assert_eq!(
        app.ui.input.queue,
        vec![InputItem::Message("queued".into())],
        "queue intact while an approval is up"
    );
}

/// A session-scoped Command (/clear) typed in the old session is dropped
/// on swap, not carried to auto-execute in the new one. The user typed it for
/// the old session; carrying it + auto-draining would apply the old session's
/// intent to the new session (a /clear for A would clear B). /resume Commands
/// stay (switch intent is still valid); Messages stay (parked, recallable).
#[test]
fn test_drops_non_resume_commands() {
    let mut app = test_app();
    app.ui.input.queue.push(InputItem::Command("/clear".into()));
    app.ui
        .input
        .queue
        .push(InputItem::Message("kept msg".into()));
    app.ui
        .input
        .queue
        .push(InputItem::Command("/resume sid-c".into()));
    app.ui
        .input
        .queue
        .push(InputItem::Command("/rewind".into()));
    app.ui
        .input
        .queue
        .push(InputItem::Message("kept msg 2".into()));

    app.switch_session(test_bundle().0);

    // Messages + /resume stay; /clear + /rewind dropped.
    assert_eq!(
        app.ui.input.queue,
        vec![
            InputItem::ParkedMessage("kept msg".into()),
            InputItem::Command("/resume sid-c".into()),
            InputItem::ParkedMessage("kept msg 2".into()),
        ],
        "messages + /resume carried; session-scoped commands dropped; \
         messages demoted to ParkedMessage (new runner's server queue is empty)"
    );
    assert!(
        app.sessions.current.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s)
                if s.contains("dropped 2 command(s)") && s.contains("/clear") && s.contains("/rewind")
        )),
        "a hint must name the dropped commands:\n{:?}",
        app.sessions.current.transcript
    );
}

/// A clean run end with N queued Messages batches: the head spawns a fresh
/// run + the rest are InjectUser'd into it (one run, N messages). The rest
/// stay in pending (Message) until QueuedInputCommitted removes them when the
/// drive_loop drains them at the next turn boundary. Stops at a non-Message.
#[test]
fn test_clean_end_drains_messages() {
    let mut app = test_app();
    app.sessions.current.last_run_final = true;
    app.ui.input.queue.push(InputItem::Message("m1".into()));
    app.ui.input.queue.push(InputItem::Message("m2".into()));
    app.ui.input.queue.push(InputItem::Message("m3".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(dirty, "batch drain flagged dirty");
    assert!(app.agent_busy(), "head (m1) spawned a run");
    assert_eq!(
        app.ui.input.queue,
        vec![
            InputItem::Message("m2".into()),
            InputItem::Message("m3".into()),
        ],
        "rest stay in pending (InjectUser'd into the new run; QueuedInputCommitted \
         removes them when the drive_loop drains at the next turn boundary)"
    );
}

/// The batch stops at a Command: Messages behind the head are InjectUser'd,
/// but a Command after them is NOT injected (it drains singly on the next
/// idle_drain — slash commands need per-command dispatch, not mid-turn
/// injection).
#[test]
fn test_batch_stops_at_command() {
    let mut app = test_app();
    app.sessions.current.last_run_final = true;
    app.ui.input.queue.push(InputItem::Message("m1".into()));
    app.ui.input.queue.push(InputItem::Message("m2".into()));
    app.ui.input.queue.push(InputItem::Command("/clear".into()));
    app.ui.input.queue.push(InputItem::Message("m3".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(app.agent_busy(), "head (m1) spawned a run");
    assert_eq!(
        app.ui.input.queue,
        vec![
            InputItem::Message("m2".into()),
            InputItem::Command("/clear".into()),
            InputItem::Message("m3".into()),
        ],
        "m2 InjectUser'd (stays for QueuedInputCommitted); /clear + m3 untouched \
         (batch stops at the Command)"
    );
}

/// One foldable call + its result, the smallest transcript that folds into a
/// group (key call-1#0).
fn folded_transcript() -> Vec<TranscriptLine> {
    let line = |name: &str| TranscriptLine::Tool {
        name: name.to_string(),
        tool: name.to_string(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: "call-1".to_string(),
        body: String::new(),
        is_diff: false,
    };
    vec![
        TranscriptLine::User("kept".into()),
        line("bash"),
        line("result"),
    ]
}

/// Whether the fold group call-1#0 renders its body rows. An expanded group
/// renders a summary header plus its lines; a collapsed one renders the
/// summary alone.
fn group_body_visible(app: &App) -> bool {
    crate::fold::display_slots(
        &app.sessions.current.transcript,
        app.sessions.current.transcript.fold_groups(),
        &app.ui.transcript_view.expanded.fold_groups,
        false,
    )
    .iter()
    .any(|slot| matches!(slot, DisplaySlot::Line(_, Some(key)) if key == "call-1#0"))
}

/// A switch parks the open expansion sets with the session that owns them and
/// restores the entry for the session being entered, so leaving a session and
/// coming back keeps what the user had open. Keys never cross: a key open in
/// one session stays closed in another, including a key string both sessions
/// use (call-1#0), and a key the other session added (second-only) must not
/// appear after the return. Thinking keys split by identity: a key held on a
/// log entry survives the round trip, which a rebuilt session reads back as
/// the same entry; a key held on a frame position drops, because the rebuilt
/// session holds no row at that position.
#[test]
fn test_swap_restores_open_keys() {
    let (first, first_startup) = test_bundle();
    let first_sid = first.session.clone();
    let mut app = build_app(first, first_startup);
    app.sessions.current.transcript = folded_transcript().into();
    {
        let view = &mut app.ui.transcript_view;
        view.toggle_result("call-1".into());
        view.toggle_fold("call-1#0".into());
        view.toggle_thinking("1".into());
        view.toggle_thinking(RowId::local(9));
        view.toggle_subagent("child-1".into());
    }
    assert!(
        group_body_visible(&app),
        "the open group draws its body rows before the switch"
    );

    let (second, _second_startup) = test_bundle();
    let second_sid = second.session.clone();
    assert_ne!(
        first_sid, second_sid,
        "the two bundles are distinct sessions"
    );
    app.switch_session(second);
    {
        let sets = &app.ui.transcript_view.expanded;
        assert!(
            sets.results.is_empty()
                && sets.fold_groups.is_empty()
                && sets.thinking.is_empty()
                && sets.subagents.is_empty(),
            "the session entered starts with nothing open: {:?} {:?} {:?} {:?}",
            sets.results,
            sets.fold_groups,
            sets.thinking,
            sets.subagents
        );
    }
    // The second session opens the same group key plus a key of its own.
    app.sessions.current.transcript = folded_transcript().into();
    assert!(
        !group_body_visible(&app),
        "the entered session starts collapsed, so the check below tells the sets apart"
    );
    {
        let view = &mut app.ui.transcript_view;
        view.toggle_fold("call-1#0".into());
        view.toggle_result("second-only".into());
    }
    assert!(group_body_visible(&app));

    let (mut back, _back_startup) = test_bundle();
    back.session = first_sid;
    app.switch_session(back);
    app.sessions.current.transcript = folded_transcript().into();
    let sets = &app.ui.transcript_view.expanded;
    assert_eq!(
        sets.results.len(),
        1,
        "the first session's own keys come back: {:?}",
        sets.results
    );
    assert!(sets.results.contains("call-1"));
    assert!(
        !sets.results.contains("second-only"),
        "the second session's keys stay with it: {:?}",
        sets.results
    );
    assert!(
        sets.fold_groups.contains("call-1#0"),
        "the group the user had open comes back: {:?}",
        sets.fold_groups
    );
    assert!(
        sets.thinking.contains(&"1".into()),
        "a thinking key held on a log entry comes back: the rebuilt session \
         reads the same entry: {:?}",
        sets.thinking
    );
    assert!(
        !sets.thinking.contains(&RowId::local(9)),
        "a thinking key held on a frame position does not come back: the \
         rebuilt session holds no row at it: {:?}",
        sets.thinking
    );
    assert!(sets.subagents.contains("child-1"));
    assert!(
        group_body_visible(&app),
        "the returned session draws the group the user had open"
    );
}

/// A session switch taken while a child view is open must park only the
/// parent's own sets under the leaving session. The rows open in the child
/// view belong to the child; restoring them into the parent on the way
/// back would expand rows the parent reader never opened.
#[test]
fn test_switch_parking_hygiene() {
    let mut app = test_app();
    let first_sid = app.sessions.current.identity.session_id.clone();
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "first".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    app.ui
        .transcript_view
        .expanded
        .results
        .insert("parent-key".into());
    assert!(app.enter_teammate_view_for_sid("c1", false));
    // A row opened while the child view is on screen.
    app.ui
        .transcript_view
        .expanded
        .results
        .insert("child-key".into());

    app.switch_session(test_bundle().0);
    assert!(
        app.ui.teammate_view.is_none(),
        "the switch leaves the child view behind"
    );

    let (mut back, _back_startup) = test_bundle();
    back.session = first_sid;
    app.switch_session(back);
    assert!(
        app.ui
            .transcript_view
            .expanded
            .results
            .contains("parent-key"),
        "the parent's own open rows come back: {:?}",
        app.ui.transcript_view.expanded.results
    );
    assert!(
        !app.ui
            .transcript_view
            .expanded
            .results
            .contains("child-key"),
        "the child's open rows do not ride into the parent: {:?}",
        app.ui.transcript_view.expanded.results
    );
}

/// A child view state parked in the session being left does not cross the
/// switch: the new session's fleet is unrelated, and a child id can repeat
/// across sessions, so a surviving entry would restore into a different
/// child's view.
#[test]
fn test_switch_drops_child_park() {
    let mut app = test_app();
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "first".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    assert!(app.enter_teammate_view_for_sid("c1", false));
    app.ui
        .transcript_view
        .expanded
        .results
        .insert("child-key".into());
    app.exit_teammate_view();

    app.switch_session(test_bundle().0);
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "unrelated".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    assert!(app.enter_teammate_view_for_sid("c1", false));
    assert!(
        app.ui.transcript_view.expanded.results.is_empty(),
        "the new session's child starts with no parked view state: {:?}",
        app.ui.transcript_view.expanded.results
    );
}

/// A swap does not reset the process-level resources the terminal owns:
/// terminal focus and the clipboard. The old rebuild reset them via app()'s
/// defaults, impersonating a cleanup of the terminal. The no-rebuild swap
/// leaves them alone so a focus loss survives a session switch.
#[test]
fn test_swap_preserves_terminal_focus() {
    let mut app = test_app();
    app.ui.terminal_focused = false;
    app.switch_session(test_bundle().0);
    assert!(
        !app.ui.terminal_focused,
        "terminal_focused preserved, not reset to the app() default"
    );
}

/// A swap resets verbose: it is a search-view lifecycle flag (set on enter,
/// cleared on exit), not a process resource. A switch out of an open search
/// view must clear it, or the new session renders expanded and untruncated
/// with no search view open. The old rebuild cleared it via app()'s defaults.
#[test]
fn test_swap_resets_verbose() {
    let mut app = test_app();
    app.ui.verbose = true;
    app.switch_session(test_bundle().0);
    assert!(
        !app.ui.verbose,
        "verbose reset to false, not carried from the search view"
    );
}

/// A swap carries last_run_final from its pre-switch value rather than
/// forging it true. The old rebuild reset it false via app()'s defaults then
/// re-set it true so the carried queue would drain -- a forged run result.
/// The no-rebuild swap keeps the true value; the carried queue drains via
/// the idle drain's switched flag instead.
#[test]
fn test_swap_carries_run_final() {
    let mut app = test_app();
    app.sessions.current.last_run_final = false;
    app.switch_session(test_bundle().0);
    assert!(
        !app.sessions.current.last_run_final,
        "last_run_final carried from the pre-switch value, not forged true"
    );
    // The true case distinguishes carry from reset-false: a reset would drop
    // it to false, a forge would set it true regardless. Carry keeps the
    // pre-switch value, so true stays true.
    let mut app2 = test_app();
    app2.sessions.current.last_run_final = true;
    app2.switch_session(test_bundle().0);
    assert!(
        app2.sessions.current.last_run_final,
        "true carried, not reset to false"
    );
}

/// A swap drains a carried queued message even when last_run_final is false:
/// the idle drain's switched flag treats the swap itself as a clean
/// transition (it runs at idle after a user pick or a clean end), so the
/// carried item drains in the new session without a forged run result. This
/// is the replacement for the old forged last_run_final=true.
#[test]
fn test_switch_drains_carried_queue() {
    let mut app = test_app();
    app.sessions.current.last_run_final = false;
    enqueue_resume_target(&mut app, "sid-target");
    app.ui
        .input
        .queue
        .push(InputItem::Message("carried".into()));
    let mut dirty = false;
    let builder: Box<ResumeBuilderRef> = Box::new(|_| Ok(test_bundle()));
    app.idle_drain(Some(&*builder), &mut dirty);
    assert!(dirty, "swap + drain flagged dirty");
    assert!(queued_target(&app).is_none(), "resume target consumed");
    assert!(
        app.ui.input.queue.is_empty(),
        "carried message drained via the switched gate, not a forged last_run_final"
    );
}
