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

/// Idle (not busy) with queued inputs: Esc pulls the whole queue back into
/// the input box in order (joined by newlines), so the user can edit the
/// batch and resubmit. Commands stay queued. No running task to abort, so
/// the cancel-when-idle priority does not apply.
#[test]
fn test_idle_esc_recalls_all() {
    let mut app = working();
    app.pending.push(PendingItem::Message("task a".into()));
    app.pending.push(PendingItem::Message("task b".into()));
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.input.value(),
        "task a\ntask b",
        "Esc recalls all in order"
    );
    assert!(app.pending.is_empty(), "queue drained to input");
}

/// While a run is in flight with queued inputs, the first Esc interrupts
/// (the queue stays intact, the draft untouched); the second Esc pulls the
/// whole queue back in order. Splitting interrupt from recall stops a
/// panic double-press from destroying the just-recalled text: the old
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
    assert_eq!(
        app.input.value(),
        "task a\ntask b",
        "second Esc recalls all in order"
    );
    assert!(app.pending.is_empty(), "tail drained to input");
}

/// Parked messages (no server copy, blocked behind a barrier or orphaned by
/// an interrupt) are recalled alongside live messages — joined in queue
/// order, no QueueRemove fired (there is no server copy to drop).
#[test]
fn test_esc_recalls_parked() {
    let mut app = working();
    app.pending
        .push(PendingItem::ParkedMessage("held a".into()));
    app.pending
        .push(PendingItem::ParkedMessage("held b".into()));
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.input.value(),
        "held a\nheld b",
        "parked messages recalled in order"
    );
    assert!(app.pending.is_empty(), "queue drained");
}

/// Slash commands stay queued when messages are recalled — joining them
/// would let a leading slash reparse the batch as a command and lose the
/// messages. The command surfaces as the strip head after the messages leave.
#[test]
fn test_esc_recall_keeps_command() {
    let mut app = working();
    app.pending.push(PendingItem::Message("do work".into()));
    app.pending.push(PendingItem::Command("/clear".into()));
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.input.value(), "do work", "only the message is recalled");
    assert_eq!(
        app.pending,
        vec![PendingItem::Command("/clear".into())],
        "command stays queued"
    );
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

/// Queued inputs render in the bounded footer strip above the input box (not
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
        out.contains("→ next"),
        "queue strip must render, got:\n{out}"
    );
    assert!(
        out.contains("· 2."),
        "second item shows position glyph, got:\n{out}"
    );
    assert!(
        out.contains("fix the bug") && out.contains("run tests"),
        "both queued items previewed, got:\n{out}"
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
        out.contains("→ next"),
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

/// A click on a footer-strip preview item recalls that item into the input:
/// removed from the queue, loaded to the input.
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

/// A click on the second item row (n=2, no overflow) recalls that item,
/// exercising the shown=2 branch of the click handler — distinct from the
/// n>2 path where row 1 is the "+N more" summary.
#[test]
fn test_click_second_row_recalls() {
    let mut app = working();
    app.pending.push(PendingItem::Message("first task".into()));
    app.pending.push(PendingItem::Message("second task".into()));
    render_buffer(&app, 100, 28);
    let qrect = app.queue_rect.get();
    assert!(qrect.height >= 2, "two-item queue gets two rows");
    let click = mouse_at(qrect.x + 2, qrect.y + 1);
    crate::app::handle_mouse(&mut app, click);
    assert_eq!(
        app.input.value(),
        "second task",
        "click on the second row recalls the second item"
    );
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("first task".into())],
        "recalled item removed, first stays"
    );
}

/// The queue strip renders above the input box. Guards the layout move by
/// scanning the rendered text: the → queue row must sit above the ❯ input
/// prompt row.
#[test]
fn test_queue_row_above_input() {
    let mut app = working();
    app.pending.push(PendingItem::Message("one".into()));
    app.pending.push(PendingItem::Message("two".into()));
    let text = render_text(&app, 100, 28);
    let mut q = None;
    let mut p = None;
    for (i, line) in text.lines().enumerate() {
        if q.is_none() && line.contains('→') {
            q = Some(i);
        }
        if p.is_none() && line.contains('❯') {
            p = Some(i);
        }
    }
    let q = q.expect("queue strip row rendered");
    let p = p.expect("input prompt row rendered");
    assert!(q < p, "queue row {} must sit above input row {}", q, p);
}

/// A small window collapses the strip to a one-line count: the head glyph
/// plus the total, no per-item rows.
#[test]
fn test_queue_summary_one_row() {
    let mut app = working();
    app.pending.push(PendingItem::Message("a".into()));
    app.pending.push(PendingItem::Message("b".into()));
    app.pending.push(PendingItem::Message("c".into()));
    let out = render_text(&app, 80, 18);
    assert!(out.contains("→ +3"), "small window count summary: {out}");
}

/// A parked message (no server copy, blocked behind a barrier or orphaned)
/// shows the held glyph rather than next/n.
#[test]
fn test_queue_held_row() {
    let mut app = working();
    app.pending
        .push(PendingItem::ParkedMessage("blocked msg".into()));
    let out = render_text(&app, 100, 28);
    assert!(out.contains("⏸ held"), "parked shows held glyph: {out}");
    assert!(out.contains("blocked msg"), "parked body shown: {out}");
}

/// A click on the +N more row (or the one-line summary on small windows)
/// pulls the whole queue back into the input box in order — same as Esc
/// recall, not a single-item recall.
#[test]
fn test_click_more_recalls_all() {
    let mut app = working();
    app.pending.push(PendingItem::Message("a".into()));
    app.pending.push(PendingItem::Message("b".into()));
    app.pending.push(PendingItem::Message("c".into()));
    app.pending.push(PendingItem::Message("d".into()));
    render_buffer(&app, 100, 28);
    let qrect = app.queue_rect.get();
    assert!(qrect.height >= 2, "strip has the head row + a +N row");
    let more_row = qrect.y + 1;
    let click = mouse_at(qrect.x + 2, more_row);
    crate::app::handle_mouse(&mut app, click);
    assert_eq!(app.input.value(), "a\nb\nc\nd", "+N row pulls all in order");
    assert!(app.pending.is_empty(), "queue drained on +N click");
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
/// injection. The pending copy is what the strip renders + what the run-end
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
