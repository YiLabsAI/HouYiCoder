//! Interaction tests for the flow-completion features: task auto-start into
//! design, the convergence rework loop (review->implement, verify->implement),
//! rewind un-approve + targeted rewind, and the verify failure path. Each test
//! renders the App and asserts on real output.

#![cfg(test)]

use crate::pending_queue::PendingItem;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use houyicoder_protocol::frontend::SlashCommand;
use ratatui::style::Color;

use crate::composition;
use crate::state::{Divergence, Pane, Screen, Stage, TranscriptLine};
use crate::test_support::{render_buffer, render_text};

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

#[test]
fn test_queue_overlay_toggle() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending.push(PendingItem::Message("task b".into()));
    assert!(!app.queue_view_open);
    crate::keys::handle_working(&mut app, ctrl('g'));
    assert!(app.queue_view_open, "Ctrl+G opens the queue overlay");
    assert_eq!(app.queue_focus, 0, "focus resets to first on open");
    crate::keys::handle_working(&mut app, ctrl('g'));
    assert!(!app.queue_view_open, "second Ctrl+G closes");
}

#[test]
fn test_empty_queue_no_overlay() {
    let mut app = working();
    crate::keys::handle_working(&mut app, ctrl('g'));
    assert!(
        !app.queue_view_open,
        "Ctrl+G must not open on an empty queue"
    );
}

#[test]
fn test_queue_overlay_recall_focused() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending.push(PendingItem::Message("task b".into()));
    app.queue_view_open = true;
    app.queue_focus = 1;
    crate::keys::handle_working(&mut app, key('e'));
    assert!(!app.queue_view_open, "e closes the overlay");
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("task a".into())],
        "focused item removed"
    );
    assert_eq!(app.input.value(), "task b", "focused item loaded to input");
}

#[test]
fn test_queue_overlay_delete_stays() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending.push(PendingItem::Message("task b".into()));
    app.pending.push(PendingItem::Message("task c".into()));
    app.queue_view_open = true;
    app.queue_focus = 1;
    crate::keys::handle_working(&mut app, key('d'));
    assert!(app.queue_view_open, "d stays open when items remain");
    assert_eq!(
        app.pending,
        vec![
            PendingItem::Message("task a".into()),
            PendingItem::Message("task c".into())
        ],
        "focused deleted"
    );
    assert_eq!(app.input.value(), "", "d does not load the input box");
}

#[test]
fn test_queue_overlay_recall_all() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending.push(PendingItem::Message("task b".into()));
    app.queue_view_open = true;
    crate::keys::handle_working(&mut app, key('a'));
    assert!(!app.queue_view_open, "a closes the overlay");
    assert!(app.pending.is_empty(), "a clears the queue");
    assert!(app.input.value().contains("task a"));
    assert!(app.input.value().contains("task b"));
}

/// Idle (not busy) with a queued input: Esc recalls the queue HEAD into the
/// input box for editing — the cancel-when-idle priority (there is no
/// running task to abort). The head leaves the queue; the tail stays. This
/// is the Esc-at-idle path; the Ctrl+G overlay's 'e' stays the path for a
/// non-head (focused) item.
#[test]
fn test_idle_esc_recalls_head() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending.push(PendingItem::Message("task b".into()));
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.input.value(),
        "task a",
        "Esc should recall the queue head"
    );
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("task b".into())],
        "head leaves, tail stays"
    );
}

/// While a run is in flight with a queued input, the first Esc interrupts
/// (the queue stays intact, the draft untouched); the second Esc recalls
/// the head into the input box. Splitting interrupt from recall stops a
/// panic double-press from destroying the just-recalled message: the old
/// combined abort+pop left agent_busy true after the abort, so the second
/// Esc fell through to clear-input and wiped the popped text.
#[test]
fn test_busy_esc_recall() {
    let mut app = working();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending.push(PendingItem::Message("task b".into()));
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    crate::keys::handle_working(&mut app, esc);
    assert!(app.cancelling, "first Esc interrupts the run");
    assert!(
        app.input.is_empty(),
        "first Esc does not touch the input box"
    );
    assert_eq!(
        app.pending,
        vec![
            PendingItem::Message("task a".into()),
            PendingItem::Message("task b".into())
        ],
        "the queue stays intact after the interrupt"
    );
    crate::keys::handle_working(&mut app, esc);
    assert_eq!(app.input.value(), "task a", "second Esc recalls the head");
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("task b".into())],
        "tail stays queued (head recalled to input)"
    );
}

#[test]
fn test_focus_clears_queue_overlay() {
    // Regression: a stale queue_view_open flag (opened in Working, then the
    // stage advanced into Focus) used to silently capture keys in Focus while
    // the overlay was not rendered. The flag must self-heal on the first
    // non-Working key so no invisible capture happens.
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.queue_view_open = true;
    app.viewport = crate::state::ViewportMode::Focus;
    crate::keys::handle_working(&mut app, key('a'));
    assert!(
        !app.queue_view_open,
        "stale overlay flag self-heals in Focus (no invisible capture)"
    );
}

#[test]
fn test_palette_blocks_queue_overlay() {
    // Ctrl+G must not open the queue overlay while the palette is open, or
    // two overlays would stack and steal each other's keys.
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.open_palette();
    crate::keys::handle_working(&mut app, ctrl('g'));
    assert!(!app.queue_view_open, "Ctrl+G suppressed while palette open");
    assert!(app.palette.open, "palette stays open");
}

#[test]
fn test_auto_start_task_enters() {
    let mut app = working();
    app.input.set("fix the login bug".to_string());
    app.submit_input();
    assert_eq!(app.stage, Stage::Design, "task should auto-start design");
    assert_eq!(app.pane, Pane::Spec);
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptLine::System(s)) if s.contains("drafting design")
        ),
        "should log the design-draft transition"
    );
}

/// Every submission — including slash commands — must leave a visible User
/// turn in the transcript before its response, so issuing /context or /debug
/// is a real interaction record, not a side-channel that only shows the
/// result.
#[test]
fn test_command_echoes_user_turn() {
    let mut app = working();
    app.input.set("/debug".to_string());
    app.submit_input();
    let echoed = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(s) if s == "/debug"));
    assert!(
        echoed,
        "/debug must echo as a User turn before its response"
    );
    assert!(
        matches!(app.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("debug")),
        "the debug response should follow the echoed command"
    );
}

/// Queued inputs render in the bounded footer strip below the input box (not
/// as transcript tail rows), so a long queue never eats the interaction view.
/// Regression guard for the strip going invisible (budget=0 or wrong mode).
/// Regression: the input border + prompt glyph used to dim to DarkGray while
/// busy. They must stay stable (border Gray, glyph Cyan) — the busy signal is
/// the OSC 9;4 chrome + spinner row, not input dimming.
#[test]
fn test_input_stable_while_busy() {
    let mut app = working();
    app.agent_busy = true;
    let buf = render_buffer(&app, 100, 28);
    let glyph = buf
        .content()
        .iter()
        .find(|c| c.symbol() == "\u{276f}")
        .expect("prompt glyph rendered");
    assert_eq!(
        glyph.style().fg,
        Some(Color::Cyan),
        "prompt glyph must not dim while busy"
    );
    let border = buf
        .content()
        .iter()
        .find(|c| c.symbol() == "\u{2500}")
        .expect("input border rendered");
    assert_eq!(
        border.style().fg,
        Some(Color::Gray),
        "input border must not dim while busy"
    );
}

#[test]
fn test_queue_strip_renders() {
    let mut app = working();
    app.pending.push(PendingItem::Message("fix the bug".into()));
    app.pending.push(PendingItem::Message("run tests".into()));
    let out = render(&app);
    assert!(
        out.contains("queued:"),
        "queue strip must render, got:\n{out}"
    );
    assert!(
        out.contains("fix the bug") && out.contains("run tests"),
        "both queued items previewed, got:\n{out}"
    );
}

/// The Ctrl+G queue overlay covers the transcript with the full list + the
/// action footer — per-item edit/del lives here.
#[test]
fn test_queue_overlay_covers_transcript() {
    let mut app = working();
    app.pending.push(PendingItem::Message("fix the bug".into()));
    app.queue_view_open = true;
    let out = render(&app);
    assert!(
        out.contains("queue  (e edit"),
        "overlay header renders, got:\n{out}"
    );
    assert!(
        out.contains("fix the bug"),
        "overlay lists the item, got:\n{out}"
    );
}

/// Regression for the Focus-mode queue-invisibility bug: the strip must render
/// in Focus mode too (the old inline render lived in the shared transcript;
/// after moving it to the footer it was lost from Focus/Scroll until the
/// footer cell was added to those layouts).
#[test]
fn test_queue_strip_in_focus() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending.push(PendingItem::Message("task b".into()));
    app.stage = Stage::Implementing;
    app.pane = Pane::Diff;
    app.viewport = crate::state::ViewportMode::Focus;
    let out = render(&app);
    assert!(
        out.contains("queued:"),
        "queue strip renders in Focus, got:\n{out}"
    );
}

#[test]
fn test_rewind_unapproves_artifact() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // spec approved -> plan
    assert!(app.spec_artifact.approved);
    app.run_command(SlashCommand::Rewind);
    assert_eq!(app.stage, Stage::Design);
    assert!(
        !app.spec_artifact.approved,
        "rewind should un-approve the spec artifact"
    );
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptLine::System(s)) if s.contains("un-approved")
        ),
        "should log the un-approve note"
    );
}

#[test]
fn test_rewind_targeted_to_named() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // -> plan
    app.approve_in_pane(); // -> implement
    app.input.set("/rewind spec".to_string());
    app.submit_input();
    assert_eq!(app.stage, Stage::Design, "targeted rewind to design");
    assert!(!app.spec_artifact.approved);
}

#[test]
fn test_rework_real_finding() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement
    // approve all 3 changes; auto-advance walks pending changes in order and
    // trips the all-approved transition to verify.
    for _ in 0..3 {
        app.approve_in_pane();
    }
    assert_eq!(app.stage, Stage::Verify);
    // focus the real security finding (S-2 is verdict real)
    while app.review.current().is_none_or(|f| f.verdict != "real") {
        app.navigate_pane(true);
        if app.review.focus == 0 {
            break;
        }
    }
    app.rework_in_pane();
    assert_eq!(
        app.stage,
        Stage::Implementing,
        "rework from review should go back to implementing"
    );
    assert_eq!(app.pane, Pane::Diff);
    assert_eq!(
        app.spec_clauses
            .iter()
            .find(|c| c.id == "clause-2")
            .map(|c| c.status),
        Some(Divergence::Partial),
        "real finding's clause should regress to partial"
    );
}

#[test]
fn test_verify_fail_rework() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement
    // approve all 3 changes (auto-advance) -> verify, then all 3 findings
    // (review phase, navigate between findings) -> machine-check phase.
    for _ in 0..3 {
        app.approve_in_pane();
    }
    for _ in 0..3 {
        app.approve_in_pane();
        app.navigate_pane(true);
    }
    assert_eq!(app.stage, Stage::Verify);
    // Simulate a failed verify directly (no /verify-fail test hook in the
    // production dispatcher): the rework path is what matters, not the
    // trigger. verify_result.passed is the field the gate reads.
    app.verify_result.passed = false;
    app.verify_result.checks = crate::composition::failing_checks();
    assert!(!app.verify_result.passed);
    // 'a' cannot complete on failure
    app.approve_in_pane();
    assert_eq!(app.stage, Stage::Verify, "cannot complete on failed checks");
    // 'r' rework -> back to implementing
    app.rework_in_pane();
    assert_eq!(
        app.stage,
        Stage::Implementing,
        "verify rework should go back to implementing"
    );
    let out = render(&app);
    println!("--- after verify rework ---\n{out}\n--- end ---");
}

// --- queue overlay combo-operation tests ---

/// Ctrl+U (clear-to-line-start) must pass through the overlay to the input
/// handler so the user can clear the input box without closing the overlay.
/// Regression: the overlay used to swallow every non-command key via its
/// catch-all match arm, leaving Ctrl+U dead while the overlay was open.
#[test]
fn test_overlay_ctrl_u_passes() {
    let mut app = working();
    app.input.set("old text".to_string());
    app.pending.push(PendingItem::Message("task a".into()));
    app.queue_view_open = true;
    crate::keys::handle_working(&mut app, ctrl('u'));
    assert!(
        app.input.is_empty(),
        "Ctrl+U must clear the input while the overlay is open"
    );
    assert!(
        app.queue_view_open,
        "overlay must stay open after Ctrl+U (pass-through, not close)"
    );
}

/// Ctrl+A / Ctrl+E / Left / Right / Backspace also pass through the overlay so
/// the user can position the cursor and delete chars while managing the queue.
#[test]
fn test_overlay_edit_keys_pass() {
    let mut app = working();
    app.input.set("hello".to_string());
    app.pending.push(PendingItem::Message("task a".into()));
    app.queue_view_open = true;
    // Backspace deletes a char.
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert_eq!(app.input.value(), "hell", "Backspace passes through");
    // Left moves the cursor (no exception, no swallow).
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    // Right moves back.
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(app.queue_view_open, "overlay stays open after nav keys");
}

/// Ctrl+G closes the overlay when it is already open (same gesture toggles).
/// Regression: Ctrl+G was swallowed by the overlay catch-all arm because the
/// overlay-capture block ran before the Ctrl+G toggle check.
#[test]
fn test_overlay_ctrl_g_closes() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.queue_view_open = true;
    crate::keys::handle_working(&mut app, ctrl('g'));
    assert!(!app.queue_view_open, "Ctrl+G must close the open overlay");
}

/// Enter in the overlay closes it and falls through to submit — it does NOT
/// recall the focused item. Recall is the e key alone. This preserves the
/// muscle memory that Enter = send.
#[test]
fn test_overlay_enter_submits() {
    let mut app = working();
    app.input.set("typed task".to_string());
    app.pending.push(PendingItem::Message("queued a".into()));
    app.pending.push(PendingItem::Message("queued b".into()));
    app.queue_view_open = true;
    app.queue_focus = 1;
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.queue_view_open, "Enter closes the overlay");
    assert_eq!(
        app.pending.len(),
        2,
        "Enter must not recall (queue untouched)"
    );
    // The typed input was submitted: stub path echoes a User turn.
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "typed task")),
        "Enter fell through to submit the typed input"
    );
}

/// Enter in the overlay with an empty input closes the overlay and no-ops
/// (empty-submit guard), so the user is not stuck.
#[test]
fn test_overlay_enter_empty_noops() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.queue_view_open = true;
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.queue_view_open, "Enter closes the overlay");
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("task a".into())],
        "queue untouched on empty submit"
    );
}

/// Bare printable chars that are not overlay commands (e/d/a) are swallowed so
/// the user does not accidentally type into the input while managing the queue.
#[test]
fn test_overlay_bare_chars_swallowed() {
    let mut app = working();
    app.input.clear();
    app.pending.push(PendingItem::Message("task a".into()));
    app.queue_view_open = true;
    crate::keys::handle_working(&mut app, key('x'));
    assert!(
        app.input.is_empty(),
        "bare x must not type into the input while overlay open"
    );
    assert!(app.queue_view_open, "overlay stays open");
}

/// A click on a footer-strip preview item recalls that item into the input
/// (same as e on it): removed from the queue, loaded to the input.
#[test]
fn test_click_footer_recalls_item() {
    let mut app = working();
    app.pending.push(PendingItem::Message("first task".into()));
    app.pending.push(PendingItem::Message("second task".into()));
    // Render so queue_rect is stashed.
    render_buffer(&app, 100, 28);
    let qrect = app.queue_rect.get();
    assert!(qrect.height > 0, "queue strip rendered with a rect");
    // Click the first item row (row 0 inside the strip).
    let click = mouse_at(qrect.x + 2, qrect.y);
    crate::app::handle_mouse(&mut app, click);
    assert_eq!(
        app.input.value(),
        "first task",
        "click on first preview recalls it"
    );
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("second task".into())],
        "recalled item removed from queue"
    );
}

/// A click on the +N more row (or the one-line summary on small windows) opens
/// the full overlay instead of recalling an item.
#[test]
fn test_click_more_row_opens() {
    let mut app = working();
    app.pending.push(PendingItem::Message("a".into()));
    app.pending.push(PendingItem::Message("b".into()));
    app.pending.push(PendingItem::Message("c".into()));
    app.pending.push(PendingItem::Message("d".into()));
    render_buffer(&app, 100, 28);
    let qrect = app.queue_rect.get();
    assert!(qrect.height >= 2, "strip has item rows + a +N row");
    // The +N more row is the row after the two preview items.
    let more_row = qrect.y + 2;
    let click = mouse_at(qrect.x + 2, more_row);
    crate::app::handle_mouse(&mut app, click);
    assert!(app.queue_view_open, "click on +N row opens the overlay");
    assert_eq!(app.queue_focus, 0, "focus starts at the top");
    assert!(app.input.is_empty(), "no item recalled on +N click");
}

/// A click inside the open overlay focuses the clicked item (no recall — the
/// user decides with e/d/a). The click must not start a transcript selection.
#[test]
fn test_click_overlay_focuses_item() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending.push(PendingItem::Message("task b".into()));
    app.pending.push(PendingItem::Message("task c".into()));
    app.queue_view_open = true;
    render_buffer(&app, 100, 28);
    let rect = app.transcript_rect.get();
    // Overlay layout: row 0 header, row 1 blank, row 2 = item 0, row 3 = item 1.
    let item1_row = rect.y + 3;
    let click = mouse_at(rect.x + 2, item1_row);
    crate::app::handle_mouse(&mut app, click);
    assert_eq!(app.queue_focus, 1, "click focuses the clicked item");
    assert!(app.queue_view_open, "overlay stays open after focus click");
    assert!(
        !app.selection.has_selection(),
        "overlay click must not start a transcript selection"
    );
}

/// When the queue drains to empty while the overlay flag is still open, the
/// overlay must not render an empty list (no stale empty overlay flash).
#[test]
fn test_empty_queue_hides_overlay() {
    let mut app = working();
    app.pending.clear();
    app.queue_view_open = true;
    let out = render(&app);
    assert!(
        !out.contains("queue  (e edit"),
        "no overlay header when queue is empty, got:\n{out}"
    );
}

/// Recalling an item (e) removes it from the queue and loads it to the input.
/// Clearing the recalled input (Ctrl+U) loses the item — this is expected
/// (remove-on-recall, matching a typical editor). Submitting empty no-ops.
/// Documented here as a behavior contract, not a bug.
#[test]
fn test_recall_then_clear_loses() {
    let mut app = working();
    app.pending
        .push(PendingItem::Message("important task".into()));
    app.queue_view_open = true;
    crate::keys::handle_working(&mut app, key('e'));
    assert_eq!(app.input.value(), "important task");
    assert!(app.pending.is_empty(), "item removed on recall");
    // Clear the recalled input.
    crate::keys::handle_working(&mut app, ctrl('u'));
    assert!(app.input.is_empty(), "Ctrl+U clears the recalled text");
    assert!(
        app.pending.is_empty(),
        "item is gone (remove-on-recall is by design)"
    );
    // Submitting empty no-ops.
    app.submit_input();
    assert!(
        app.transcript.is_empty(),
        "empty submit no-ops (no User turn recorded)"
    );
}

/// Deleting the last remaining item closes the overlay and resets focus.
#[test]
fn test_delete_last_item_closes() {
    let mut app = working();
    app.pending.push(PendingItem::Message("only task".into()));
    app.queue_view_open = true;
    app.queue_focus = 0;
    crate::keys::handle_working(&mut app, key('d'));
    assert!(!app.queue_view_open, "overlay closes when queue empties");
    assert!(app.pending.is_empty());
}

/// queue_focus never points past the end after a delete in the middle: it
/// clamps to the new last index so the next e/d acts on a valid item.
#[test]
fn test_delete_middle_clamps_focus() {
    let mut app = working();
    for s in ["a", "b", "c"] {
        app.pending.push(PendingItem::Message(s.into()));
    }
    app.queue_view_open = true;
    app.queue_focus = 2; // last item
    crate::keys::handle_working(&mut app, key('d')); // delete "c"
    assert_eq!(app.queue_focus, 1, "focus clamps to new last index");
    // Next e recalls "b" (the now-last item).
    crate::keys::handle_working(&mut app, key('e'));
    assert_eq!(app.input.value(), "b");
}

fn mouse_at(x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

// --- mid-turn injection (session/inject + session/queue_remove wires) ---

use crate::agent_message::AgentMessage;

/// While a run is busy, a submit copies the input to pending (the
/// queue view) + ships a session/inject so the host enqueues it for mid-turn
/// injection. The pending copy is what Ctrl+G renders + what the run-end
/// drain spawns if the run ends before the next turn boundary consumes it.
#[test]
fn test_busy_submit_mirrors_queue() {
    let mut app = working();
    app.agent_busy = true;
    app.spawn_run("first interjection".into());
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("first interjection".into())],
        "busy submit lands in the queue",
    );
    // A second submit while still busy appends (FIFO).
    app.spawn_run("second interjection".into());
    assert_eq!(app.pending.len(), 2, "FIFO queue order");
}

/// While a teammate view is open, a submit steers to the viewed child rather
/// than starting a parent turn: no parent run starts (agent_busy stays false)
/// and no parent transcript echo lands.
#[test]
fn test_teammate_submit_steers() {
    let mut app = working();
    app.teammate_view = Some(crate::records::TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    app.spawn_run("focus on auth".into());
    assert!(!app.agent_busy, "steering does not start a parent run");
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(_))),
        "no parent echo for a steering submit"
    );
    assert!(
        app.pending.is_empty(),
        "steering does not queue on the parent"
    );
}

/// A QueueConsumed event (the host reports which queued texts the drive loop
/// injected this run) removes the matching entry from the pending copy — a consumed
/// message is no longer pending, so the queue view + run-end drain stay
/// accurate (no double-spawn at run end).
#[test]
fn test_consumed_removes_from_mirror() {
    let mut app = working();
    app.pending.push(PendingItem::Message("alpha".into()));
    app.pending.push(PendingItem::Message("beta".into()));
    app.handle_agent_message(AgentMessage::QueueConsumed {
        texts: vec!["alpha".to_string()],
    });
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("beta".into())],
        "consumed entry removed from the copy",
    );
}

/// Overlay delete ships session/queue_remove (so the host drops it too) +
/// removes the item from the pending copy. The wire construction line executes even
/// with no backend wired (send_cmd is a no-op then); the pending copy is the
/// observable effect.
#[test]
fn test_overlay_delete_wires_remove() {
    let mut app = working();
    app.pending.push(PendingItem::Message("item a".into()));
    app.pending.push(PendingItem::Message("item b".into()));
    app.queue_view_open = true;
    crate::keys::handle_working(&mut app, key('d'));
    assert!(
        !app.pending.contains(&PendingItem::Message("item a".into())),
        "deleted focused item removed from the copy",
    );
    assert_eq!(app.pending, vec![PendingItem::Message("item b".into())]);
}

/// Overlay recall (e) ships session/queue_remove (the user pulled the item
/// back to the input box, so it is no longer queued) + removes it from the
/// pending copy.
#[test]
fn test_overlay_recall_wires_remove() {
    let mut app = working();
    app.pending.push(PendingItem::Message("item a".into()));
    app.queue_view_open = true;
    crate::keys::handle_working(&mut app, key('e'));
    assert!(
        app.pending.is_empty(),
        "recalled item removed from the copy"
    );
    assert!(!app.queue_view_open, "overlay closed on recall");
}
