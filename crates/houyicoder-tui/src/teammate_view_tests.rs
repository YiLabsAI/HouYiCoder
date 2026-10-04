//! State isolation between the parent transcript and teammate views:
//! the open-row sets and the scroll position belong to the view on screen,
//! a sibling child inherits nothing from the last one, and the parent's
//! full-history search stays gated while a child is on view. The gate
//! covers both directions: the keyboard cannot open a search under a
//! child, and the mouse cannot open a child under a search.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::agent_message::FleetEntry;
use crate::app::handle_mouse;
use crate::records::TranscriptLine;
use crate::state::App;
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};

fn user_frame(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

/// A delegation line whose folded child transcript tags every row with the
/// child id, so two children never share row text.
fn subagent_line(sid: &str, rows: usize) -> TranscriptLine {
    TranscriptLine::Subagent {
        child_sid: sid.into(),
        subagent_type: "explore".into(),
        summary: "summary".into(),
        prompt: "task".into(),
        folded_transcript: (0..rows)
            .map(|i| TranscriptLine::Agent(format!("{sid} row {i}")))
            .collect(),
        color: None,
    }
}

/// A working app whose parent transcript holds one user line per frame,
/// followed by two delegation lines with loaded child transcripts.
fn app_with_two_children(parent_rows: usize) -> App {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.runtime = Some(crate::composition::shared_runtime());
    for i in 0..parent_rows {
        app.transcript
            .push_frame(user_frame(&format!("parent row {i}")));
    }
    app.rebuild_transcript();
    app.transcript.push(subagent_line("c1", 40));
    app.transcript.push(subagent_line("c2", 40));
    app
}

/// A row opened while one child is on view must not be open when its sibling
/// comes up: the two children are separate views, and the thinking keys
/// are per-log turn counters that repeat across children by construction.
#[test]
fn test_child_expansion_isolation() {
    let mut app = app_with_two_children(5);
    assert!(app.enter_teammate_view_for_sid("c1", false));
    app.expanded_thinking.insert("1".into());
    app.exit_teammate_view();

    assert!(app.enter_teammate_view_for_sid("c2", false));
    assert!(
        app.expanded_thinking.is_empty(),
        "a sibling child starts with its own empty sets, not the last child's keys"
    );
}

/// The parent's open rows are held while a child is on view and reinstalled
/// on the way out: the child must not render the parent's expansion, and
/// the parent must find its expansion exactly as the reader left it.
#[test]
fn test_parent_expansion_survives() {
    let mut app = app_with_two_children(5);
    app.expanded_results.insert("call-p".into());

    assert!(app.enter_teammate_view_for_sid("c1", false));
    assert!(
        !app.expanded_results.contains("call-p"),
        "the child view does not carry the parent's open rows"
    );

    app.exit_teammate_view();
    assert!(
        app.expanded_results.contains("call-p"),
        "the parent's expansion is restored on exit"
    );
}

/// Re-entering a child resumes at the viewport the reader parked: the same
/// rows render, and the scroll is off the tail where the reader left it.
#[test]
fn test_child_scroll_restored() {
    let mut app = app_with_two_children(60);
    assert!(app.enter_teammate_view_for_sid("c1", false));
    // Publish the child totals so the page steps move by a full screen.
    let _first = crate::test_harness::render_text(&app, 80, 24);
    app.scroll_transcript_up();
    app.scroll_transcript_up();
    let parked = crate::test_harness::render_text(&app, 80, 24);
    let top = app
        .teammate_view
        .as_ref()
        .expect("child on view")
        .scroll
        .raw_top();

    app.exit_teammate_view();
    assert!(app.enter_teammate_view_for_sid("c1", false));
    let view = app.teammate_view.as_ref().expect("child on view");
    assert_eq!(
        view.scroll.raw_top(),
        top,
        "the re-entered child resumes at the parked offset"
    );
    let back = crate::test_harness::render_text(&app, 80, 24);
    assert_eq!(
        parked, back,
        "the re-entered child renders the parked viewport"
    );
}

/// The full-history search reads the parent session's durable log; opening
/// it while a child is on view would match against the parent and render
/// against the child, and a match jump would move the hidden parent's
/// viewport. The gate refuses the entry, says why, and changes nothing.
#[test]
fn test_child_blocks_search() {
    let mut app = app_with_two_children(60);
    app.transcript_scroll.jump_to(10);
    let top = app.transcript_scroll.raw_top();

    assert!(app.enter_teammate_view_for_sid("c1", false));
    app.enter_search_view("row");

    assert!(
        !app.search.active,
        "search stays closed while a child is on view"
    );
    assert!(app.teammate_view.is_some(), "the child view stands");
    assert_eq!(
        app.transcript_scroll.raw_top(),
        top,
        "the hidden parent's viewport did not move"
    );
    assert!(
        app.notifications.current().is_some(),
        "the refusal is told to the reader, not silent"
    );
}

/// A direct hop from one child to another, the way the fleet pane enters a
/// selected row, passes through the same exit and entry as the gesture
/// path: the second child inherits nothing, the hop exits to the parent's
/// own sets, and the first child's state is parked for its re-entry.
#[test]
fn test_sibling_view_swap() {
    let mut app = app_with_two_children(5);
    app.expanded_results.insert("parent-key".into());

    assert!(app.enter_teammate_view_for_sid("c1", false));
    app.expanded_thinking.insert("a1".into());
    assert!(app.enter_teammate_view_for_sid("c2", false));
    assert_eq!(
        app.teammate_view.as_ref().expect("child on view").child_sid,
        "c2",
        "the hop lands on the second child"
    );
    assert!(
        app.expanded_thinking.is_empty(),
        "the second child does not inherit the first child's keys"
    );

    app.exit_teammate_view();
    assert!(
        app.expanded_results.contains("parent-key"),
        "the hop exits through the parent's own sets"
    );
    assert!(
        !app.expanded_thinking.contains("a1"),
        "the first child's key does not ride out to the parent"
    );

    assert!(app.enter_teammate_view_for_sid("c1", false));
    assert!(
        app.expanded_thinking.contains("a1"),
        "re-entering the first child restores what it had open"
    );
}

/// The expand key acts on the visible view: ctrl+o while a child view is
/// open toggles the child's reasoning row, and the expanded text renders in
/// the view.
#[test]
fn test_ctrl_o_expands_child() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.runtime = Some(crate::composition::shared_runtime());
    app.transcript.push_frame(user_frame("parent row"));
    app.rebuild_transcript();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "summary".into(),
        prompt: "task".into(),
        folded_transcript: vec![
            TranscriptLine::ThoughtFor {
                ms: Some(59),
                reasoning: Some("CHILDTHINK weighing options".into()),
                tool_summary: None,
                turn_id: "f3".into(),
            },
            TranscriptLine::Agent("child result".into()),
        ],
        color: None,
    });
    assert!(app.enter_teammate_view_for_sid("c1", false));
    let before = crate::test_harness::render_text(&app, 100, 30);
    assert!(
        !before.contains("CHILDTHINK"),
        "the row starts collapsed:\n{before}"
    );
    crate::keys::handle_ctrl_o(&mut app);
    assert!(
        app.expanded_thinking.contains("f3"),
        "the toggle targets the child row, set: {:?}",
        app.expanded_thinking
    );
    let after = crate::test_harness::render_text(&app, 100, 30);
    assert!(after.contains("CHILDTHINK"), "the row expands:\n{after}");
}

/// The search gate covers the mouse as well as the keyboard: a click that
/// lands where the fleet strip was drawn before the search opened does not
/// enter a child view. Every frame clears the strip's click target before
/// the screen dispatch, so the router sees an empty rect wherever the
/// search view renders and the keyboard gate cannot be walked around.
#[test]
fn test_search_ignores_fleet_click() {
    let mut app = app_with_two_children(5);
    for sid in ["c1", "c2"] {
        app.fleet.entries.push(FleetEntry {
            agent_id: sid.into(),
            subagent_type: "explore".into(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
            started_at: None,
        });
    }
    let _working = crate::test_harness::render_text(&app, 80, 24);
    let strip = app.fleet.rect.get();
    assert!(
        strip.width > 0 && strip.height > 0,
        "the strip is a click target while working: {strip:?}"
    );

    app.enter_search_view("row");
    let _search = crate::test_harness::render_text(&app, 80, 24);
    let cleared = app.fleet.rect.get();
    assert!(
        cleared.width == 0 && cleared.height == 0,
        "the search frame clears the strip's click target: {cleared:?}"
    );

    // Two clicks where the strip stood: the first would select a row and
    // the second would enter its child view, if the old rect still routed.
    for _ in 0..2 {
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: strip.x,
                row: strip.y + 1,
                modifiers: KeyModifiers::NONE,
            },
        );
    }
    assert!(
        app.teammate_view.is_none(),
        "a click on the old strip does not open a child view during search"
    );
    assert!(app.search.active, "the search view stands");
}
