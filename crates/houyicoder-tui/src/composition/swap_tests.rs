use super::*;
use crate::pending_queue::PendingItem;
use crate::state::TranscriptLine;

fn test_bundle() -> RunnerBundle {
    let bundle = houyicoder_service::composition::build_runner(None, None, None);
    let runner = bundle.runner;
    let session = bundle.session;
    let gate = bundle.gate;
    let append_notify = bundle.append_notify;
    let wire_session = houyicoder_protocol::frontend::SessionId(session.to_string());
    let (tx, rx) = mpsc::channel::<crate::run_control::AgentMessage>();
    let (runner, client, startup_warnings) =
        pair_inproc_server(runner, session, gate, append_notify);
    drop(runner);
    RunnerBundle {
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
    }
}

/// Like test_bundle but returns the server task's JoinHandle so a test can
/// assert the serve loop exited after the session is torn down.
fn test_bundle_tracked() -> (RunnerBundle, tokio::task::JoinHandle<()>) {
    let bundle = houyicoder_service::composition::build_runner(None, None, None);
    let runner = bundle.runner;
    let session = bundle.session;
    let gate = bundle.gate;
    let append_notify = bundle.append_notify;
    let wire_session = houyicoder_protocol::frontend::SessionId(session.to_string());
    let (tx, rx) = mpsc::channel::<crate::run_control::AgentMessage>();
    let (runner, client, serve, startup_warnings) =
        pair_inproc_server_tracked(runner, session, gate, append_notify);
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
            session_lister: None,
            skip_login: false,
            startup_warnings,
        },
        serve,
    )
}

#[test]
fn test_switch_session_resets_view() {
    let mut app = build_app(test_bundle());
    app.transcript
        .push(TranscriptLine::User("old content".into()));
    app.frames.push(crate::transcript::TranscriptFrame::Session(
        houyicoder_protocol::frontend::session_update::SessionUpdate::UserMessageChunk(
            houyicoder_protocol::frontend::session_update::ContentChunk::new(
                houyicoder_protocol::frontend::run::ContentBlock::Text {
                    text: "old frame".into(),
                },
            ),
        ),
    ));
    let new_bundle = test_bundle();
    let new_sid = new_bundle.session.0.clone();
    let new_warning_count = new_bundle.startup_warnings.len();
    app.swap_session(new_bundle);
    // swap clears the old transcript; the new bundle's startup warnings
    // (e.g. a sandbox-fence notice) land as initial system lines, so the
    // transcript has exactly that many lines, not the old content.
    assert_eq!(
        app.transcript.len(),
        new_warning_count,
        "transcript cleared of old content, only new startup warnings remain"
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("old content"))),
        "old content gone"
    );
    assert!(app.frames.is_empty(), "frames cleared");
    assert_eq!(app.session_id.0, new_sid, "session_id updated");
    assert!(app.session.is_some(), "new session driver wired");
    assert!(!app.agent_busy, "agent_busy cleared");
    assert!(app.transcript_scroll.follow_tail, "scroll reset");
    assert_eq!(app.pane, crate::state::Pane::Transcript, "pane reset");
}

#[test]
fn test_swap_bumps_transcript_version() {
    let mut app = build_app(test_bundle());
    let v_before = app.transcript_version.get();
    app.swap_session(test_bundle());
    assert_ne!(
        app.transcript_version.get(),
        v_before,
        "version bumped to invalidate cache"
    );
}

/// build_app pushes the bundle's startup_warnings as initial transcript
/// system lines (synchronous, before any run output). A bad settings value
/// surfaces as a system line instead of silently dropping.
#[test]
fn test_build_app_surfaces_warnings() {
    let mut bundle = test_bundle();
    bundle
        .startup_warnings
        .push("model.effort_level: bad".into());
    bundle
        .startup_warnings
        .push("sandbox.network: unknown".into());
    let app = build_app(bundle);
    let lines: Vec<&str> = app
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
    let mut app = build_app(test_bundle());
    let mut dirty = false;
    app.try_swap_session(None, &mut dirty);
    assert!(!dirty, "no target = no-op");
    assert!(!app.quit, "no target = no-op");
}

/// The old busy put-back branch is gone: the event loop's idle guard
/// (!agent_busy && !reverse_request_in_flight) now gates try_swap_session,
/// so it only runs when idle. A test that set agent_busy=true + called
/// try_swap_session directly would exercise a path the caller no longer
/// reaches; the idle-guarded swap is covered by try_swap_swaps_when_idle
/// + the PTY journeys (resume_sid_reopens_history, resume_picker_swaps).

#[test]
fn test_try_no_builder_quits() {
    let mut app = build_app(test_bundle());
    app.pending_resume_target = Some("some-sid".to_string());
    let mut dirty = false;
    app.try_swap_session(None, &mut dirty);
    assert!(!dirty, "no builder = no swap");
    assert!(app.quit, "no builder = fallback quit");
    assert_eq!(
        app.pending_resume_target.as_deref(),
        Some("some-sid"),
        "target put back for caller"
    );
}

/// Adversarial: a resume_builder that returns Err surfaces a system line +
/// does NOT swap (the session stays), does NOT quit (unlike the no-builder
/// fallback). A builder failure (disk read, sid resolution, provider) must
/// not crash or silently no-op — the user is told, the current session is
/// preserved, and the target is consumed (not retried every frame).
#[test]
fn test_resume_err_keeps_session() {
    let mut app = build_app(test_bundle());
    app.pending_resume_target = Some("bad-sid".to_string());
    let mut dirty = false;
    let builder: Box<ResumeBuilderRef> = Box::new(|_| Err("sid not on disk".into()));
    app.try_swap_session(Some(&*builder), &mut dirty);
    assert!(dirty, "the error system line flags dirty");
    assert!(
        !app.quit,
        "a builder error is not a quit (unlike no builder)"
    );
    assert!(
        app.pending_resume_target.is_none(),
        "target consumed on error (no retry-every-frame storm)"
    );
    assert!(
        app.transcript.iter().any(
            |l| matches!(l, crate::state::TranscriptLine::System(s) if s.contains("resume failed"))
        ),
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
    let mut app = build_app(test_bundle());
    app.status.last_run_final = true;
    app.pending.push(PendingItem::Message("head".into()));
    let mut dirty = false;
    // No resume target + idle + clean end: drain the queued head.
    app.idle_drain(None, &mut dirty);
    assert!(dirty, "drain flagged dirty");
    assert!(app.pending.is_empty(), "head drained");
}

/// Busy (a run in flight): idle_drain is a no-op -- neither swap nor
/// drain. The continuous-state poll runs every frame but consumes nothing.
#[test]
fn test_idle_drain_noop_busy() {
    let mut app = build_app(test_bundle());
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("queued".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(!dirty, "busy = no drain");
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("queued".into())],
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
    let mut app = build_app(test_bundle());
    app.status.last_run_final = false;
    app.pending
        .push(PendingItem::ParkedMessage("parked".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(!dirty, "no drain on a non-clean end (interrupt/error)");
    assert_eq!(
        app.pending,
        vec![PendingItem::ParkedMessage("parked".into())],
        "queued item parks (not auto-sent) -- recallable via Esc"
    );
}

/// A clean run end (FinalOutput) drains a parked head: the queued item
/// auto-sends as the next turn. The parked path (no server copy) spawns a
/// fresh run; this is the drain_pending_head ParkedMessage branch under a
/// wired session.
#[test]
fn test_clean_end_drains_parked() {
    let mut app = build_app(test_bundle());
    app.status.last_run_final = true;
    app.pending
        .push(PendingItem::ParkedMessage("parked".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(dirty, "drain on a clean end (FinalOutput)");
    assert!(app.pending.is_empty(), "parked head drained (spawn_run)");
}

/// A deferred resume target with a queued message: idle_drain acts on the
/// resume target first (try_swap_session runs before drain_pending_head in
/// idle_drain), so the swap happens before the message drains. The swap
/// carries the pending queue across (demoting Messages to ParkedMessage --
/// the new runner's server queue is empty), then the head drains in the new
/// session, auto-sending the carried message as the first turn in B.
#[test]
fn test_resume_precedes_queued_message() {
    let mut app = build_app(test_bundle());
    app.pending_resume_target = Some("sid-target".to_string());
    app.pending.push(PendingItem::Message("queued msg".into()));
    let mut dirty = false;
    let builder: Box<ResumeBuilderRef> = Box::new(|_| Ok(test_bundle()));
    app.idle_drain(Some(&*builder), &mut dirty);
    assert!(dirty, "swap flagged dirty");
    assert!(
        app.pending_resume_target.is_none(),
        "resume target consumed before the message drains"
    );
    assert!(
        app.pending.is_empty(),
        "carried message drained in the new session (auto-sent as B's first turn)"
    );
}

/// swap_session must clear session-local state: the old session's todo
/// list, token/step counts, pending approval, text selection, and the
/// row caches for selection/copy. None of those survive a swap -- the
/// new session must render as if it were fresh, not layered over the old
/// one's todos/tokens/selection pointing at stale rows. The pending queue
/// is NOT in this set: it is carried across (see swap_carries_pending).
#[test]
fn test_clears_session_local_state() {
    use crate::todo_view::{TodoStatus, TodoView};
    let mut app = build_app(test_bundle());
    // Populate with old-session residue.
    app.todos_cache.push(TodoView {
        content: "old todo".into(),
        status: TodoStatus::InProgress,
        active_form: None,
    });
    app.cumulative_tokens = 1234;
    app.cumulative_steps = 7;
    app.displayed_tokens.set(50);
    app.status.tokens = 999;
    app.approval = Some(crate::records::Approval::default());
    app.selection.anchor = Some((0, 5));
    app.selection.is_dragging = true;
    app.last_all_rows.borrow_mut().push((0, "stale row".into()));
    app.last_row_callids
        .borrow_mut()
        .push(Some("stale-call".into()));
    // Sanity: the residue is there before the swap.
    assert!(!app.todos_cache.is_empty());
    assert!(app.selection.anchor.is_some());

    app.swap_session(test_bundle());

    assert!(app.todos_cache.is_empty(), "todos cleared");
    assert_eq!(app.cumulative_tokens, 0, "cumulative_tokens cleared");
    assert_eq!(app.cumulative_steps, 0, "cumulative_steps cleared");
    assert_eq!(app.displayed_tokens.get(), 0, "displayed_tokens cleared");
    assert_eq!(app.status.tokens, 0, "status.tokens cleared");
    assert!(app.approval.is_none(), "approval cleared");
    assert!(app.selection.anchor.is_none(), "selection anchor cleared");
    assert!(!app.selection.is_dragging, "selection drag cleared");
    assert!(
        app.last_all_rows.borrow().is_empty(),
        "last_all_rows cleared"
    );
    assert!(
        app.last_row_callids.borrow().is_empty(),
        "last_row_callids cleared"
    );
}

/// swap_session carries the pending queue across the swap, demoting Messages
/// to ParkedMessage (the new runner's server queue is empty, so a live server copy
/// is lost). A command ahead of messages stays ahead; the queue order is
/// preserved. The carried items auto-drain in the new session on the next
/// idle (a queued message from A sends in B as the next turn), unless a Command barrier holds.
#[test]
fn test_swap_carries_pending_across() {
    let mut app = build_app(test_bundle());
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending
        .push(PendingItem::Command("/resume sid-b".into()));
    app.pending.push(PendingItem::Message("task c".into()));

    app.swap_session(test_bundle());

    assert_eq!(
        app.pending,
        vec![
            PendingItem::ParkedMessage("task a".into()),
            PendingItem::Command("/resume sid-b".into()),
            PendingItem::ParkedMessage("task c".into()),
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
    let (bundle, old_serve) = test_bundle_tracked();
    let mut app = build_app(bundle);
    // The old server is alive before the swap (serve loop waiting on input
    // after the handshake settles).
    let mut tries = 0;
    while !old_serve.is_finished() && tries < 25 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        tries += 1;
    }
    assert!(
        !old_serve.is_finished(),
        "old server alive before swap (handshake done, waiting on input)"
    );
    // Swap: build_app resets self, dropping the old Session. The driver
    // JoinHandle is detached (the task is not aborted), but the driver exits
    // when its command channel returns None.
    app.swap_session(test_bundle());
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
    use houyicoder_protocol::envelope::RequestId;
    let mut app = build_app(test_bundle());
    app.pending_permission_req_id.set(Some(RequestId(42)));
    app.pending.push(PendingItem::Message("queued".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(!dirty, "approval pending = no drain and no swap");
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("queued".into())],
        "queue intact while an approval is up"
    );
}

/// P1: a session-scoped Command (/clear) typed in the old session is dropped
/// on swap, not carried to auto-execute in the new one. The user typed it for
/// the old session; carrying it + auto-draining would apply the old session's
/// intent to the new session (a /clear for A would clear B). /resume Commands
/// stay (switch intent is still valid); Messages stay (parked, recallable).
#[test]
fn test_drops_non_resume_commands() {
    let mut app = build_app(test_bundle());
    app.pending.push(PendingItem::Command("/clear".into()));
    app.pending.push(PendingItem::Message("kept msg".into()));
    app.pending
        .push(PendingItem::Command("/resume sid-c".into()));
    app.pending.push(PendingItem::Command("/rewind".into()));
    app.pending.push(PendingItem::Message("kept msg 2".into()));

    app.swap_session(test_bundle());

    // Messages + /resume stay; /clear + /rewind dropped.
    assert_eq!(
        app.pending,
        vec![
            PendingItem::ParkedMessage("kept msg".into()),
            PendingItem::Command("/resume sid-c".into()),
            PendingItem::ParkedMessage("kept msg 2".into()),
        ],
        "messages + /resume carried; session-scoped commands dropped; \
         messages demoted to ParkedMessage (new runner's server queue is empty)"
    );
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            crate::state::TranscriptLine::System(s)
                if s.contains("dropped 2 command(s)") && s.contains("/clear") && s.contains("/rewind")
        )),
        "a hint must name the dropped commands:\n{:?}",
        app.transcript
    );
}

/// A clean run end with N queued Messages batches: the head spawns a fresh
/// run + the rest are InjectUser'd into it (one run, N messages). The rest
/// stay in pending (Message) until QueueConsumed removes them when the
/// drive_loop drains them at the next turn boundary. Stops at a non-Message.
#[test]
fn test_clean_end_drains_messages() {
    let mut app = build_app(test_bundle());
    app.status.last_run_final = true;
    app.pending.push(PendingItem::Message("m1".into()));
    app.pending.push(PendingItem::Message("m2".into()));
    app.pending.push(PendingItem::Message("m3".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(dirty, "batch drain flagged dirty");
    assert!(app.agent_busy, "head (m1) spawned a run");
    assert_eq!(
        app.pending,
        vec![
            PendingItem::Message("m2".into()),
            PendingItem::Message("m3".into()),
        ],
        "rest stay in pending (InjectUser'd into the new run; QueueConsumed \
         removes them when the drive_loop drains at the next turn boundary)"
    );
}

/// The batch stops at a Command: Messages behind the head are InjectUser'd,
/// but a Command after them is NOT injected (it drains singly on the next
/// idle_drain — slash commands need per-command dispatch, not mid-turn
/// injection).
#[test]
fn test_batch_stops_at_command() {
    let mut app = build_app(test_bundle());
    app.status.last_run_final = true;
    app.pending.push(PendingItem::Message("m1".into()));
    app.pending.push(PendingItem::Message("m2".into()));
    app.pending.push(PendingItem::Command("/clear".into()));
    app.pending.push(PendingItem::Message("m3".into()));
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(app.agent_busy, "head (m1) spawned a run");
    assert_eq!(
        app.pending,
        vec![
            PendingItem::Message("m2".into()),
            PendingItem::Command("/clear".into()),
            PendingItem::Message("m3".into()),
        ],
        "m2 InjectUser'd (stays for QueueConsumed); /clear + m3 untouched \
         (batch stops at the Command)"
    );
}
