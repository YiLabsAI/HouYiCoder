use super::*;

/// A Subagent delegation line renders inline in the parent flow (no context
/// switch): collapsed shows the subagent type + summary + an expand hint.
/// Pins the inline-fold render so a refactor that drops the Subagent arm (or
/// reverts to a context-switch) fails here.
#[test]
fn test_subagent_renders_collapsed() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth module".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("explore"), "subagent type renders: {out}");
    assert!(out.contains("found auth module"), "summary renders: {out}");
    assert!(
        out.contains("ctrl+o to expand"),
        "collapsed shows the expand hint: {out}"
    );
}

/// When the child_sid is in expanded_subagents, the Subagent line renders the
/// collapse hint + a placeholder for the unloaded child transcript. The
/// fetch that fills folded_transcript lands next; this pins the expanded
/// branch so a refactor that drops it fails here.
#[test]
fn test_subagent_renders_expanded() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth module".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.expanded_subagents.insert("child-1".into());
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("ctrl+o to collapse"),
        "expanded shows the collapse hint: {out}"
    );
    assert!(
        out.contains("child transcript not yet loaded"),
        "expanded shows the placeholder until the fetch lands: {out}"
    );
}

/// When expanded and the child transcript is loaded, the Subagent line
/// renders the child's rows inline (recursively through the same row
/// builder). Pins the recursive render branch so a refactor that drops it
/// fails here.
#[test]
fn test_subagent_expanded_renders_child() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth module".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply: auth is here".into())],
        color: None,
    });
    app.expanded_subagents.insert("child-1".into());
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("child reply: auth is here"),
        "expanded with a loaded child renders the child row inline: {out}"
    );
}

/// Ctrl+O toggles the last Subagent delegation's expand state. The first
/// call expands (expanded_subagents gains the child_sid); the second
/// collapses (removed). Pins the toggle wiring so a refactor that drops it
/// fails here.
#[test]
fn test_subagent_toggle_expand() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    assert!(app.expanded_subagents.is_empty(), "starts collapsed");
    assert!(app.toggle_tail_expand(), "toggle returns true");
    assert!(
        app.expanded_subagents.contains("child-1"),
        "first toggle expands"
    );
    assert!(app.toggle_tail_expand(), "toggle returns true again");
    assert!(app.expanded_subagents.is_empty(), "second toggle collapses");
}

/// Feeding a ChildTranscriptResult fills the matching Subagent line's
/// folded_transcript so the expanded render shows the fetched child rows.
/// Pins the fetch-result to update path (lookup by child_sid + in-place swap).
#[test]
fn test_child_transcript_fills() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.expanded_subagents.insert("child-1".into());
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "child-1".into(),
        frames: vec![tool_call_frame(
            "c1",
            "grep auth",
            ToolCallStatus::Completed,
        )],
    });
    let folded = app
        .transcript
        .iter()
        .find_map(|l| match l {
            TranscriptLine::Subagent {
                child_sid,
                folded_transcript,
                ..
            } if child_sid == "child-1" => Some(folded_transcript.clone()),
            _ => None,
        })
        .expect("subagent line present");
    assert!(
        !folded.is_empty(),
        "ChildTranscriptResult fills folded_transcript, got empty"
    );
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.to_lowercase().contains("grep auth"),
        "expanded render shows the fetched child row: {out}"
    );
}

/// A toggle taken after a first render must change what the next render
/// shows, in both directions. The row cache is keyed on a content version,
/// and subagent expansion was missing from that key: the state flipped, the
/// screen did not, so expand looked dead and collapse impossible — until an
/// unrelated input invalidated the cache and the block appeared on its own.
/// The existing render tests all seeded the expand state before the first
/// render, where the cache is cold and rebuilds unconditionally, so none of
/// them could see this. Render first, then toggle.
#[test]
fn test_subagent_toggle_repaints() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::User("task".into()));
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply here".into())],
        color: None,
    });
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        !out.contains("child reply here"),
        "first render is collapsed: {out}"
    );
    app.toggle_tail_expand();
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("child reply here"),
        "expand must repaint the child rows: {out}"
    );
    app.toggle_tail_expand();
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        !out.contains("child reply here"),
        "collapse must repaint too: {out}"
    );
}

/// A fetched child transcript lands by swapping the payload into the existing
/// Subagent line, which skips the push path that invalidates the row cache.
/// Render the expanded-but-unloaded state first so the cache is warm, then
/// feed the reply: the fetched rows must appear on the next render rather
/// than waiting for an unrelated change to bump the version.
#[test]
fn test_child_fetch_repaints() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.expanded_subagents.insert("child-1".into());
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("not yet loaded"),
        "warm the cache on the unloaded state: {out}"
    );
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "child-1".into(),
        frames: vec![tool_call_frame(
            "c1",
            "grep auth",
            ToolCallStatus::Completed,
        )],
    });
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.to_lowercase().contains("grep auth"),
        "the fetched child rows must repaint: {out}"
    );
}

/// When no Subagent is present, Ctrl+O falls through to the ThoughtFor
/// expand path. Pins the fallthrough so a refactor that drops it fails.
#[test]
fn test_ctrl_o_fallthrough() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::ThoughtFor {
        secs: 3,
        reasoning: Some("pondered the task".into()),
        tool_summary: None,
        turn_id: "t1".into(),
    });
    assert!(app.expanded_thinking.is_empty());
    crate::keys::handle_ctrl_o(&mut app);
    assert!(
        app.expanded_thinking.contains("t1"),
        "falls through to ThoughtFor when no Subagent is present"
    );
}

/// With no Subagent and no ThoughtFor but an active todo list, Ctrl+O
/// expands the collapsed checklist. Pins the todo fallthrough path.
#[test]
fn test_ctrl_o_todo_expand() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.todos_cache.push(crate::todo_view::TodoView {
        content: "do the thing".into(),
        status: crate::todo_view::TodoStatus::Pending,
        active_form: None,
    });
    assert!(!app.todo_expanded);
    crate::keys::handle_ctrl_o(&mut app);
    assert!(app.todo_expanded, "Ctrl+O expands the todo list");
}

/// Expanding/collapsing a Subagent fold does not shift the content row
/// indices of sibling transcript lines. The child transcript renders as
/// display rows inside the fold, not as new content rows in the parent
/// index space, so the parent's line indices stay stable (the single
/// source of truth). Pins the invariant so a refactor that reindexes on
/// expand fails here.
#[test]
fn test_subagent_expand_stable_index() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::User("task".into()));
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
        color: None,
    });
    app.transcript
        .push(TranscriptLine::Agent("parent result".into()));
    // Content row indices: User=0, Subagent=1, Agent=2.
    assert!(matches!(app.transcript[0], TranscriptLine::User(_)));
    assert!(matches!(app.transcript[1], TranscriptLine::Subagent { .. }));
    assert!(matches!(app.transcript[2], TranscriptLine::Agent(_)));
    // Expand the Subagent — the transcript Vec does not change.
    app.expanded_subagents.insert("child-1".into());
    assert!(matches!(app.transcript[0], TranscriptLine::User(_)));
    assert!(matches!(app.transcript[1], TranscriptLine::Subagent { .. }));
    assert!(matches!(app.transcript[2], TranscriptLine::Agent(_)));
    // Collapse — still stable.
    app.expanded_subagents.remove("child-1");
    assert!(matches!(app.transcript[0], TranscriptLine::User(_)));
    assert!(matches!(app.transcript[1], TranscriptLine::Subagent { .. }));
    assert!(matches!(app.transcript[2], TranscriptLine::Agent(_)));
}

/// line_display_rows must match the render row count for a Subagent:
/// 1 when collapsed, 2 when expanded with empty folded_transcript
/// (head + placeholder), and 1 + child rows when expanded with loaded
/// children. Pins the count==render invariant the scroll math depends on.
#[test]
fn test_subagent_row_count() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    let sub = TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
        color: None,
    };
    // Collapsed: 1 head row.
    assert_eq!(app.line_display_rows(&sub), 1);
    // Expanded with loaded children: 1 head + child rows.
    app.expanded_subagents.insert("c1".into());
    let child_rows = app.line_display_rows(&TranscriptLine::Agent("child reply".into()));
    assert_eq!(app.line_display_rows(&sub), 1 + child_rows);
    // Expanded with empty folded_transcript: 1 head + 1 placeholder.
    let empty_sub = TranscriptLine::Subagent {
        child_sid: "c2".into(),
        subagent_type: "explore".into(),
        summary: "no output".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    };
    app.expanded_subagents.insert("c2".into());
    assert_eq!(app.line_display_rows(&empty_sub), 2);
}
