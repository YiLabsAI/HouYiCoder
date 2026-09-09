//! Queue interface tests for recall actions, footer rendering, and the queue pane.

#![cfg(test)]

use crate::pending_queue::PendingItem;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Color;

use crate::composition;
use crate::state::{Pane, Screen, Stage};
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

/// Esc recall is destructive: once recalled to the input box, clearing the
/// input (Ctrl+U) permanently drops the message -- it is no longer in the
/// queue. This is by design (recall is an explicit user action). Pin it so a
/// future change that adds undo to recall does not silently weaken the
/// contract. The batch version (N messages) is N-wide; this test uses one
/// message since Ctrl+U kills one line at a time.
#[test]
fn test_recall_then_clear_loses() {
    let mut app = working();
    app.pending
        .push(PendingItem::Message("important task".into()));
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.input.value(),
        "important task",
        "message recalled to input"
    );
    assert!(app.pending.is_empty(), "queue drained on recall");
    // Clear the recalled input.
    crate::keys::handle_working(&mut app, ctrl('u'));
    assert!(app.input.is_empty(), "Ctrl+U clears the recalled text");
    assert!(
        app.pending.is_empty(),
        "message is gone (remove-on-recall is by design)"
    );
    // Submitting empty no-ops.
    app.submit_input();
    assert!(
        app.transcript.is_empty(),
        "empty submit no-ops (no User turn recorded)"
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
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("fix the bug".into()));
    app.pending
        .push(PendingItem::ParkedMessage("run tests".into()));
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

#[test]
fn test_multiline_preview() {
    let mut app = working();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message(
        "first line\nsecond line\nthird line".into(),
    ));
    let out = render(&app);
    assert!(
        out.contains("first line"),
        "first line remains visible:\n{out}"
    );
    assert!(out.contains("+2 lines"), "hidden lines are counted:\n{out}");
    assert!(
        !out.contains("second line"),
        "preview stays on one row:\n{out}"
    );
}

/// Regression for the Focus-mode queue-invisibility bug: the strip must render
/// in Focus mode too (the old inline render lived in the shared transcript;
/// after moving it to the footer it was lost from Focus/Scroll until the
/// footer cell was added to those layouts).
#[test]
fn test_queue_strip_in_focus() {
    let mut app = working();
    app.agent_busy = true;
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

/// A click on a footer-strip preview item recalls that item into the input:
/// removed from the queue, loaded to the input.
#[test]
fn test_click_footer_recalls_item() {
    let mut app = working();
    app.pending.push(PendingItem::Message("first task".into()));
    app.pending.push(PendingItem::Message("second task".into()));
    // Render so queue_view.strip_rect is stashed.
    render_buffer(&app, 100, 28);
    let qrect = app.queue_view.strip_rect.get();
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
    let qrect = app.queue_view.strip_rect.get();
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
    app.agent_busy = true;
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
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("a".into()));
    app.pending.push(PendingItem::Message("b".into()));
    app.pending.push(PendingItem::Message("c".into()));
    let out = render_text(&app, 80, 18);
    assert!(out.contains("→ +3"), "small window count summary: {out}");
}

/// Gate closed (idle after a non-final run end -- interrupt, max-turns,
/// error): every queued item shows held, because nothing will auto-run
/// until the user recalls (Esc) or re-sends. The strip must not claim
/// "next" -- no item holds a live copy or is about to spawn.
#[test]
fn test_queue_held_row() {
    let mut app = working();
    // working() defaults to idle + last_run_final=false (gate closed).
    app.pending
        .push(PendingItem::ParkedMessage("orphan a".into()));
    app.pending
        .push(PendingItem::ParkedMessage("orphan b".into()));
    let out = render_text(&app, 100, 28);
    assert!(
        out.contains("⏸ held"),
        "gate closed: items show held: {out}"
    );
    assert!(
        !out.contains("→ next"),
        "no live head when the gate is closed: {out}"
    );
    assert!(
        out.contains("orphan a"),
        "held items still render their body text: {out}"
    );
}

/// Gate open (a run is in flight): the live head Message is the one item
/// with a server copy -> "→ next"; a parked non-head has no copy but WILL
/// auto-run when the head's run hits a turn boundary, so it shows its
/// queue position, not "held" (held promises it will NOT auto-run).
#[test]
fn test_queue_next_row() {
    let mut app = working();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("live".into()));
    app.pending
        .push(PendingItem::ParkedMessage("queued".into()));
    let out = render_text(&app, 100, 28);
    assert!(out.contains("→ next"), "busy head shows next: {out}");
    assert!(
        out.contains("· 2."),
        "non-head shows its position (will auto-run): {out}"
    );
    assert!(
        !out.contains("⏸ held"),
        "non-head is NOT held (gate open, will auto-run): {out}"
    );
    assert!(out.contains("queued"), "queued body shown: {out}");
}

/// A click on the +N more row (or the one-line summary on small windows)
/// opens the queue pane — a non-destructive browse action, not a
/// recall-all. The queue items stay pending; the pane lets the user pick
/// one to recall or use R for explicit recall-all.
#[test]
fn test_click_more_opens_queue() {
    let mut app = working();
    app.pending.push(PendingItem::Message("a".into()));
    app.pending.push(PendingItem::Message("b".into()));
    app.pending.push(PendingItem::Message("c".into()));
    app.pending.push(PendingItem::Message("d".into()));
    render_buffer(&app, 100, 28);
    let qrect = app.queue_view.strip_rect.get();
    assert!(qrect.height >= 2, "strip has the head row + a +N row");
    let more_row = qrect.y + 1;
    let click = mouse_at(qrect.x + 2, more_row);
    crate::app::handle_mouse(&mut app, click);
    assert_eq!(
        app.pane,
        crate::state::Pane::Queue,
        "+N row opens the queue pane"
    );
    assert_eq!(
        app.pending.len(),
        4,
        "queue items are not drained by opening the pane"
    );
    assert!(
        app.input.value().is_empty(),
        "input box is not filled by opening the pane"
    );
}

fn mouse_at(x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

/// Enter in the queue pane recalls only the selected item, not all.
#[test]
fn test_pane_enter_recalls_one() {
    let mut app = working();
    app.pane = Pane::Queue;
    app.pending.push(PendingItem::Message("alpha".into()));
    app.pending.push(PendingItem::Message("beta".into()));
    app.queue_view.cursor = 1;
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.input.value(),
        "beta",
        "only the selected item is recalled"
    );
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("alpha".into())],
        "the other item stays queued",
    );
    assert_eq!(app.pane, Pane::Transcript, "pane closes after recall");
}

/// R in the queue pane recalls all items (explicit bulk action).
#[test]
fn test_pane_r_recalls_all() {
    let mut app = working();
    app.pane = Pane::Queue;
    app.pending.push(PendingItem::Message("a".into()));
    app.pending.push(PendingItem::Message("b".into()));
    app.pending.push(PendingItem::Message("c".into()));
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE),
    );
    assert_eq!(app.input.value(), "a\nb\nc", "all items recalled in order");
    assert!(app.pending.is_empty(), "queue drained");
    assert_eq!(app.pane, Pane::Transcript, "pane closes after recall-all");
}

/// d in the queue pane deletes the selected item without recalling it.
#[test]
fn test_pane_d_deletes_one() {
    let mut app = working();
    app.pane = Pane::Queue;
    app.pending.push(PendingItem::Message("alpha".into()));
    app.pending.push(PendingItem::Message("beta".into()));
    app.queue_view.cursor = 0;
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
    );
    assert!(app.input.value().is_empty(), "input not filled on delete");
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("beta".into())],
        "only the selected item is removed",
    );
    assert_eq!(app.pane, Pane::Queue, "pane stays open after delete");
}

#[test]
fn test_pane_recall_promotes_next() {
    let mut app = working();
    app.agent_busy = true;
    app.pane = Pane::Queue;
    app.pending.push(PendingItem::Message("first".into()));
    app.pending
        .push(PendingItem::ParkedMessage("second".into()));

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.pending, vec![PendingItem::Message("second".into())]);
}

#[test]
fn test_pane_delete_promotes_next() {
    let mut app = working();
    app.agent_busy = true;
    app.pane = Pane::Queue;
    app.pending.push(PendingItem::Message("first".into()));
    app.pending
        .push(PendingItem::ParkedMessage("second".into()));

    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
    );

    assert_eq!(app.pending, vec![PendingItem::Message("second".into())]);
}

#[test]
fn test_strip_recall_promotes_next() {
    let mut app = working();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("first".into()));
    app.pending
        .push(PendingItem::ParkedMessage("second".into()));
    render_buffer(&app, 100, 28);
    let qrect = app.queue_view.strip_rect.get();

    crate::app::handle_mouse(&mut app, mouse_at(qrect.x + 2, qrect.y));

    assert_eq!(app.pending, vec![PendingItem::Message("second".into())]);
}

/// Esc closes the queue pane without recalling anything.
#[test]
fn test_queue_pane_esc_closes() {
    let mut app = working();
    app.pane = Pane::Queue;
    app.pending.push(PendingItem::Message("alpha".into()));
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.pane, Pane::Transcript, "Esc closes the pane");
    assert_eq!(app.pending.len(), 1, "queue items are not touched");
}

/// The queue pane renders one bounded row per logical item, with a
/// hidden-line count for multiline messages.
#[test]
fn test_queue_pane_multiline_preview() {
    let mut app = working();
    app.pane = Pane::Queue;
    app.pending.push(PendingItem::Message(
        "line one\nline two\nline three".into(),
    ));
    app.pending.push(PendingItem::Message("short".into()));
    let text = render_text(&app, 80, 28);
    assert!(text.contains("line one"), "first line shown");
    assert!(text.contains("+2 lines"), "hidden line count shown");
    assert!(text.contains("short"), "single-line item shown");
}

/// The queue pane renders without panic at a narrow terminal width.
#[test]
fn test_queue_pane_narrow_terminal() {
    let mut app = working();
    app.pane = Pane::Queue;
    app.pending.push(PendingItem::Message(
        "a somewhat long queued message".into(),
    ));
    app.pending.push(PendingItem::Message("b".into()));
    let text = render_text(&app, 40, 20);
    assert!(text.contains("queue"), "header renders at narrow width");
}

/// Up/Down navigate the queue pane cursor.
#[test]
fn test_queue_pane_nav() {
    let mut app = working();
    app.pane = Pane::Queue;
    app.pending.push(PendingItem::Message("a".into()));
    app.pending.push(PendingItem::Message("b".into()));
    app.pending.push(PendingItem::Message("c".into()));
    app.queue_view.cursor = 0;
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.queue_view.cursor, 1, "Down moves cursor");
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.queue_view.cursor, 0, "Up moves cursor back");
}
