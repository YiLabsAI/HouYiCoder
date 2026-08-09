//! Buffer-dump tests for TUI rendering and interaction invariants. Each test
//! drives the App to a target state, renders it to a TestBackend, prints the
//! real buffer with --nocapture, and asserts on the actual rendered text so
//! behavior is confirmed from output, not from guesses.

#![cfg(test)]

use crate::pending_queue::PendingItem;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::frontend::SlashCommand;

use crate::composition;
use crate::keys;
use crate::records::{ToolOutcome, TranscriptLine};
use crate::state::{Pane, Screen, Stage, ViewportMode};
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}

// A transcript Edit result renders the structured-diff layout: a ⎿ summary
// head (Added/removed counts), then line-numbered green/red rows with the
// sigil gutter, and a dim "..." between hunks. The @@ hunk header is consumed
// as metadata and never appears as rendered text. Drives the real render
// path (result_body_rows → plan_diff_body → render_diff_plan → diff_row)
// over an authentic unified_diff body, not a hand-rolled fixture.
#[test]
fn test_transcript_diff_gutter_gap() {
    let mut app = working();
    // Two far-apart edits so the engine emits TWO hunks (Myers) — the
    // inter-hunk "..." gap is exercised only with >1 hunk, so the edits
    // must sit more than 2*context lines apart or similar merges them.
    let mut orig = String::from("fn foo() {\n    let a = 1;\n}\n");
    let mut new = String::from("fn foo() {\n    let a = ONE;\n}\n");
    for i in 0..12 {
        orig.push_str(&format!("// spacer line {i}\n"));
        new.push_str(&format!("// spacer line {i}\n"));
    }
    orig.push_str("fn bar() {\n    let b = 2;\n}\n");
    new.push_str("fn bar() {\n    let b = TWO;\n}\n");
    let diff = houyicoder_core::agent::unified_diff(&orig, &new, 3);
    let body = format!("{}\n{}", crate::brief::edit_diff_summary(2, 2), diff);
    app.transcript = vec![
        TranscriptLine::User("update foo and bar".into()),
        TranscriptLine::Tool {
            name: "result".into(),
            tool: "result".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            body,
            is_diff: true,
        },
    ];
    // Expand so the full diff (not the collapsed head) is on screen.
    app.expanded_results.insert("c1".to_string());
    let out = render_text(&app, 100, 32);
    println!("--- transcript diff render ---\n{out}\n--- end ---");
    assert!(
        out.contains("⎿  Added 2 lines, removed 2 lines"),
        "summary head must render: {out}"
    );
    // Added/removed line content surfaces (sigil gutter does not strip it).
    assert!(out.contains("let a = ONE;"), "added line content: {out}");
    assert!(out.contains("let a = 1;"), "removed line content: {out}");
    // The @@ hunk header is metadata, never rendered as text.
    assert!(!out.contains("@@ -"), "@@ header leaked into render: {out}");
    // Two hunks ⇒ the inter-hunk dim "..." gap renders.
    assert!(out.contains("..."), "inter-hunk gap missing: {out}");
}

// The green/red background fills the FULL pane width (the renderer space-
// pads each row to the terminal width so a changed line reads as a solid
// bar, not just a colored prefix). Verified at the cell level — the text
// dump trims trailing spaces so a text-only assertion cannot see the fill.
#[test]
fn test_transcript_diff_bg_fill() {
    use crate::test_support::render_buffer;
    use ratatui::style::Color;
    let mut app = working();
    let orig = "fn foo() {\n    let a = 1;\n}\n";
    let new = "fn foo() {\n    let a = ONE;\n}\n";
    let diff = houyicoder_core::agent::unified_diff(orig, new, 3);
    let body = format!("{}\n{}", crate::brief::edit_diff_summary(1, 1), diff);
    app.transcript = vec![TranscriptLine::Tool {
        name: "result".into(),
        tool: "result".into(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: "c1".into(),
        body,
        is_diff: true,
    }];
    app.expanded_results.insert("c1".to_string());
    let buf = render_buffer(&app, 100, 20);
    // Find the add row (the one whose text contains "let a = ONE;").
    let mut add_y = None;
    for y in 0..buf.area().height {
        let rowtxt: String = (0..buf.area().width)
            .map(|x| buf.cell((x, y)).expect("cell").symbol().to_string())
            .collect();
        if rowtxt.contains("let a = ONE;") {
            add_y = Some(y);
            break;
        }
    }
    let y = add_y.expect("add row rendered");
    // A cell far past the content (near the right edge) must carry the green
    // add-row background — the bar spans the full pane width, not just the text.
    let green = Color::Rgb(28, 38, 32);
    let far_right = buf.cell((98, y)).expect("cell").bg;
    assert_eq!(
        far_right, green,
        "add-row bg must fill to the right edge (got {far_right:?})"
    );
    let mid = buf.cell((50, y)).expect("cell").bg;
    assert_eq!(mid, green, "add-row bg must fill the mid row (got {mid:?})");
}

// A small inline edit (let x = 1 → let x = 2) renders word-level diff: just
// the changed word ("2") carries the darker, more-saturated word background,
// the rest of the add line carries the dim line bar background. Verified at
// the cell level — the word background differs from the line background.
#[test]
fn test_transcript_diff_word_highlight() {
    use crate::test_support::render_buffer;
    use ratatui::style::Color;
    let mut app = working();
    let orig = "fn foo() {\n    let x = 1;\n}\n";
    let new = "fn foo() {\n    let x = 2;\n}\n";
    let diff = houyicoder_core::agent::unified_diff(orig, new, 3);
    let body = format!("{}\n{}", crate::brief::edit_diff_summary(1, 1), diff);
    app.transcript = vec![TranscriptLine::Tool {
        name: "result".into(),
        tool: "result".into(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: "c1".into(),
        body,
        is_diff: true,
    }];
    app.expanded_results.insert("c1".to_string());
    let buf = render_buffer(&app, 100, 20);
    let dump = crate::test_support::dump_buffer(&buf);
    println!("--- word-diff render ---\n{dump}\n--- end ---");
    let line_bg = Color::Rgb(28, 38, 32); // dim green add-line bar
    let word_bg = Color::Rgb(46, 120, 70); // darker word-added background
    // Find the add row + locate a changed-word cell (bg = word_bg) distinct
    // from the line bar (line_bg).
    let mut add_y = None;
    for y in 0..buf.area().height {
        let rowtxt: String = (0..buf.area().width)
            .map(|x| buf.cell((x, y)).expect("cell").symbol().to_string())
            .collect();
        if rowtxt.contains("let x = 2") {
            add_y = Some(y);
            break;
        }
    }
    let y = add_y.expect("add row rendered");
    let mut has_word_cell = false;
    let mut has_line_cell = false;
    for x in 0..buf.area().width {
        let bg = buf.cell((x, y)).expect("cell").bg;
        if bg == word_bg {
            has_word_cell = true;
        } else if bg == line_bg {
            has_line_cell = true;
        }
    }
    assert!(
        has_word_cell,
        "a changed word must carry the darker word background"
    );
    assert!(
        has_line_cell,
        "the rest of the add line must carry the dim line bar background"
    );
}

// A long diff line soft-wraps across multiple rows; the continuation rows
// carry a blank gutter (no line number) with the sigil preserved, and the
// green bar background fills each wrapped row. Verified at the cell level —
// a text dump cannot see the gutter gap (blank) or the per-row bar fill.
#[test]
fn test_transcript_diff_wraps() {
    use crate::test_support::render_buffer;
    let mut app = working();
    let orig = "fn foo() {\n    let x = 1;\n}\n";
    let new = "fn foo() {\n    let x = ONE_TWO_THREE_FOUR_FIVE_SIX_SEVEN_EIGHT;\n}\n";
    let diff = houyicoder_core::agent::unified_diff(orig, new, 3);
    let body = format!("{}\n{}", crate::brief::edit_diff_summary(1, 1), diff);
    app.transcript = vec![TranscriptLine::Tool {
        name: "result".into(),
        tool: "result".into(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: "c1".into(),
        body,
        is_diff: true,
    }];
    app.expanded_results.insert("c1".to_string());
    // A narrow pane forces the long added line to wrap to multiple rows.
    let buf = render_buffer(&app, 30, 14);
    let dump = crate::test_support::dump_buffer(&buf);
    println!("--- wrapped diff render (30 cols) ---\n{dump}\n--- end ---");
    // The long add line wrapped across multiple rows, each carrying the
    // green add-line background (the bar fills every wrapped row, not just
    // the first). The remove line + context are not green, so green rows ≥ 2
    // proves the add line wrapped (≥2 add rows).
    let green = ratatui::style::Color::Rgb(28, 38, 32);
    let green_rows = (0..buf.area().height)
        .filter(|y| (0..buf.area().width).any(|x| buf.cell((x, *y)).unwrap().bg == green))
        .count();
    assert!(
        green_rows >= 2,
        "long add line must wrap to >=2 green rows, got {green_rows}"
    );
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

// Focus mode renders the diff pane full-width and hides the input box. The
// progress bar fuses into the diff pane border title.
#[test]
fn test_focus_renders_diff_full() {
    let mut app = working();
    app.set_stage(Stage::Implementing);
    assert_eq!(
        app.viewport,
        ViewportMode::Focus,
        "set_stage(Implementing) must sync viewport to Focus"
    );
    app.pane = Pane::Diff;
    let out = render_text(&app, 80, 24);
    println!("--- Focus mode (diff full-width) ---\n{out}\n--- end ---");
    assert!(
        out.contains("diff approval"),
        "Focus mode should render the diff pane full-width"
    );
    assert!(
        !out.contains("for commands"),
        "input box should be hidden in Focus mode: [{out}]"
    );
}

// The word hunk must not appear in any user-facing rendered text. The stub
// change ids, status hints, memory summary, release notes, and evidence rows
// all use change instead. Internal type names (Hunk, hunk_id field) are
// unchanged.
#[test]
fn test_no_hunk_rendered() {
    let mut app = working();
    // Drive through every screen/pane that shows change-related text.
    app.run_command(SlashCommand::Implement); // Diff pane, Focus mode
    let diff = render_text(&app, 100, 28);
    println!("--- diff pane ---\n{diff}\n--- end ---");
    assert!(
        !diff.contains("hunk"),
        "diff pane must not say hunk: {diff}"
    );
    assert!(diff.contains("change"), "diff pane should say change");

    // Review pane references the change id in its evidence line.
    app.run_command(SlashCommand::Review);
    let review = render_text(&app, 100, 28);
    println!("--- review pane ---\n{review}\n--- end ---");
    assert!(
        !review.contains("hunk"),
        "review pane must not say hunk: {review}"
    );

    // Memory pane carries the prose summary referencing changes.
    app.run_command(SlashCommand::Memory);
    let mem = render_text(&app, 100, 28);
    println!("--- memory pane ---\n{mem}\n--- end ---");
    assert!(
        !mem.contains("hunk"),
        "memory pane must not say hunk: {mem}"
    );

    // Release notes reference per-change evidence.
    app.run_command(SlashCommand::ReleaseNotes);
    let notes = render_text(&app, 100, 28);
    println!("--- release notes ---\n{notes}\n--- end ---");
    assert!(
        !notes.contains("hunk"),
        "release notes must not say hunk: {notes}"
    );

    // Console replay line references the change id.
    app.screen = Screen::Console;
    let console = render_text(&app, 100, 28);
    println!("--- console ---\n{console}\n--- end ---");
    assert!(
        !console.contains("hunk"),
        "console must not say hunk: {console}"
    );
}

// The flow must not stall in the diff pane. Pressing a repeatedly must approve
// every change in turn (auto-advancing focus to the next pending change) and
// trip the all-approved transition to verify + review pane.
#[test]
fn test_repeated_approve_advances() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement (Focus, Diff pane)
    // Three presses of a with NO manual navigation must clear all changes.
    for _ in 0..3 {
        keys::handle_working(&mut app, key(KeyCode::Char('a')));
    }
    assert_eq!(app.stage, Stage::Verify, "all changes approved -> verify");
    assert_eq!(app.pane, Pane::Review, "auto-advance should land on review");
    let out = render_text(&app, 100, 28);
    println!("--- 3x a -> verify/review ---\n{out}\n--- end ---");
    assert!(
        out.contains("review findings"),
        "review pane should be visible"
    );
    // Every change is approved.
    assert!(
        app.diff
            .hunks
            .iter()
            .all(|h| h.approved == crate::state::Verdict::Approved),
        "all changes should be approved"
    );
}

// The Scroll mode overlay must start with a SCROLL tag in Cyan + BOLD so the
// mode is unambiguous.
#[test]
fn test_scroll_overlay_tag() {
    let mut app = working();
    for _ in 0..40 {
        app.system_line("a long line of transcript history");
    }
    drop(render_text(&app, 80, 24));
    keys::handle_working(&mut app, key(KeyCode::PageUp));
    assert_eq!(app.viewport, ViewportMode::Scroll);
    let out = render_text(&app, 80, 24);
    println!("--- scroll overlay ---\n{out}\n--- end ---");
    let last = out.lines().last().expect("last row");
    assert!(
        last.starts_with(" SCROLL"),
        "scroll overlay must start with SCROLL: [{last}]"
    );
    assert!(
        last.contains("line") && last.contains("Esc=tail"),
        "scroll overlay must still show line position + tail hint: [{last}]"
    );
}

// At narrow widths (< 100 cols) the diff layout must stack rationale above
// patch (vertical) so the patch text stays readable, instead of side-by-side
// columns that crush each side to ~38 cols.
#[test]
fn test_diff_stacks_below_100() {
    let mut app = working();
    app.run_command(SlashCommand::Implement);
    // At 80 cols the patch must still be readable: the patch border title and
    // a patch line both render without being crushed into one column.
    let narrow = render_text(&app, 80, 24);
    println!("--- diff at 80 cols (stacked) ---\n{narrow}\n--- end ---");
    assert!(
        narrow.contains("patch (what)"),
        "patch column title must be present at 80 cols"
    );
    assert!(
        narrow.contains("rationale (evidence)"),
        "rationale column title must be present at 80 cols"
    );
    // The patch content (a diff hunk marker) must appear on its own line,
    // i.e. the patch area is wide enough to show real text.
    assert!(
        narrow.contains("@@"),
        "patch text must be readable at 80 cols (got crushed?):\n{narrow}"
    );

    // At 120 cols the side-by-side layout is kept (both titles on the same
    // screen, patch still readable).
    let wide = render_text(&app, 120, 24);
    println!("--- diff at 120 cols (side-by-side) ---\n{wide}\n--- end ---");
    assert!(wide.contains("patch (what)"));
    assert!(wide.contains("@@"));
}

/// A tight list followed by a paragraph: the last item must flush on its own
/// row, not weld its accumulated spans into the paragraph's first line (the
/// "list item glued to next block" regression).
#[test]
fn test_list_tail_not_welded() {
    use crate::markdown::render_agent_text;
    let (_lines, plain) = render_agent_text("●", "- apple\n- banana\n\ncherry", 80);
    let banana_row = plain.iter().position(|r| r.contains("banana")).unwrap();
    let cherry_row = plain.iter().position(|r| r.contains("cherry")).unwrap();
    assert_ne!(banana_row, cherry_row, "last item welded into next block");
    assert!(cherry_row > banana_row);
    assert!(
        !plain
            .iter()
            .any(|r| r.contains("banana") && r.contains("cherry")),
        "welded row: {plain:?}"
    );
}

/// An Edit call + its diff result must render the structured diff by default
/// (not fold the call into a "edited N files" summary that hides the diff).
/// The full line-numbered diff shows on Done; folding it made
/// the structured-diff work invisible.
#[test]
fn test_edit_diff_not_folded() {
    let mut app = working();
    let orig = "fn foo() {\n    let a = 1;\n}\n";
    let new = "fn foo() {\n    let a = ONE;\n}\n";
    let diff = houyicoder_core::agent::unified_diff(orig, new, 3);
    let body = format!("{}\n{}", crate::brief::edit_diff_summary(1, 1), diff);
    // Real flow: a call row (name "Update") + its diff result row. NOT
    // expanded_results — the default view is what we assert on.
    app.transcript = vec![
        TranscriptLine::User("update foo".into()),
        TranscriptLine::Tool {
            name: "Update".into(),
            tool: "edit".into(),
            status: "foo.rs".into(),
            invocation: "foo.rs".into(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            body: String::new(),
            is_diff: false,
        },
        TranscriptLine::Tool {
            name: "result".into(),
            tool: "result".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            body,
            is_diff: true,
        },
    ];
    let out = render_text(&app, 100, 32);
    println!("--- edit diff default render ---\n{out}\n--- end ---");
    assert!(out.contains("let a = ONE;"), "added line visible: {out}");
    assert!(out.contains("let a = 1;"), "removed line visible: {out}");
    assert!(
        !out.contains("ctrl+o to expand") && !out.contains("Edited"),
        "edit diff was folded (hidden behind summary): {out}"
    );
}

// Agent text with paragraph breaks and a fenced code block counts the same
// rows the markdown render path draws. A naive newline split overcounts
// (blank separators and code fences render to zero rows); that drift pinned
// the scroll offset past real rows. The count path must match the render
// path.
#[test]
fn test_agent_markdown_count_matches() {
    let mut app = working();
    app.transcript = vec![
        TranscriptLine::User("go".to_string()),
        TranscriptLine::Agent("para one\n\npara two\n\n```rust\nfn main() {}\n```".into()),
    ];
    let _out = render_text(&app, 80, 24);
    let count = app.transcript_display_rows();
    let rendered = app.transcript_scroll.total.get();
    assert_eq!(
        count, rendered,
        "agent markdown desync: count={count} rendered={rendered}"
    );
}

// A long agent paragraph soft-wraps at a narrow pane width, and the count
// path matches the render path so a wrapped long line does not drift the
// scroll total. Pins the markdown half of the count==render invariant.
#[test]
fn test_agent_markdown_wraps_narrow() {
    let mut app = working();
    let long = "this is a very long agent paragraph that must soft-wrap to multiple rows at a narrow pane width and not drift the count";
    app.transcript = vec![
        TranscriptLine::User("hi".to_string()),
        TranscriptLine::Agent(long.into()),
    ];
    let out = render_text(&app, 30, 24);
    let count = app.transcript_display_rows();
    let rendered = app.transcript_scroll.total.get();
    assert_eq!(
        count, rendered,
        "wrapped agent markdown desync at 30 cols: count={count} rendered={rendered}\n{out}"
    );
    assert!(
        rendered > 2,
        "long agent line must wrap to >2 rows at 30 cols, got {rendered}"
    );
}

// A long user prompt soft-wraps at a narrow pane width (angle-bracket lead
// plus the text wrapped as one block), and the count path matches the render
// path so a wrapped long prompt does not drift the scroll total. A naive
// newline split undercounts a long single-line prompt to 1.
#[test]
fn test_user_message_wraps_narrow() {
    let mut app = working();
    let long = "this is a very long user prompt that must soft-wrap to multiple rows at a narrow pane width and not drift the count";
    app.transcript = vec![TranscriptLine::User(long.into())];
    let out = render_text(&app, 30, 24);
    let count = app.transcript_display_rows();
    let rendered = app.transcript_scroll.total.get();
    assert_eq!(
        count, rendered,
        "wrapped user prompt desync: count={count} rendered={rendered}\n{out}"
    );
    assert!(
        rendered > 1,
        "long user prompt must wrap to >1 rows at 30 cols, got {rendered}"
    );
    assert!(
        out.contains("> this is a"),
        "first row carries the lead: {out}"
    );
}

// A terminal resize to a narrower width must rebuild the cached slot text at
// the new width -- otherwise the cache holds the old wide lines, and the
// narrower viewport clips their tails (content vanishes until something else
// bumps the version). area.width is in the slots-version hash for exactly
// this: the wrap width is a cached-text input.
#[test]
fn test_resize_narrow_rebuilds_tail() {
    let mut app = working();
    let tail = "ZZZ_TAIL_MARKER_ZZZ";
    let long = format!("{}{}", "x".repeat(80), tail);
    app.transcript = vec![TranscriptLine::User(long)];
    // Wide render caches the slot text at width 120 (tail on row 1, fits).
    let _wide = render_text(&app, 120, 24);
    // Narrow render: width changed -> cache rebuilds -> tail wraps to a
    // later row instead of being clipped off the stale wide line.
    let narrow = render_text(&app, 40, 24);
    assert!(
        narrow.contains(tail),
        "resize to narrower width must not drop the tail (cache rebuilt at new width):\n{narrow}"
    );
}

// A multi-line user prompt (explicit line breaks plus long lines that wrap)
// keeps count==render across logical line breaks.
#[test]
fn test_user_multiline_count_matches() {
    let mut app = working();
    let multi = "line one is long enough to wrap at this narrow width\nline two\nline three also long enough to wrap";
    app.transcript = vec![TranscriptLine::User(multi.into())];
    let out = render_text(&app, 30, 24);
    let count = app.transcript_display_rows();
    let rendered = app.transcript_scroll.total.get();
    assert_eq!(
        count, rendered,
        "multiline user prompt desync: count={count} rendered={rendered}\n{out}"
    );
}

// An expanded ThoughtFor's reasoning soft-wraps at a narrow pane width (each
// logical line indented two spaces, wrapped to width minus the indent), and
// the count path matches the render path. A naive newline-split count
// undercounts a long single-line reasoning to 1.
#[test]
fn test_thought_expand_wraps_narrow() {
    let mut app = working();
    let reasoning = "this is a very long reasoning line that must soft-wrap to multiple rows when expanded at a narrow pane width and not drift the count";
    app.transcript = vec![
        TranscriptLine::User("go".into()),
        TranscriptLine::Agent("ok".into()),
        TranscriptLine::ThoughtFor {
            secs: 3,
            reasoning: Some(reasoning.into()),
            tool_summary: None,
            turn_id: "t1".into(),
        },
    ];
    app.expanded_thinking.insert("t1".to_string());
    let out = render_text(&app, 30, 24);
    let count = app.transcript_display_rows();
    let rendered = app.transcript_scroll.total.get();
    assert_eq!(
        count, rendered,
        "expanded thought wrap desync: count={count} rendered={rendered}\n{out}"
    );
    assert!(
        out.contains("Thought for 3s"),
        "thought header present: {out}"
    );
    assert!(
        rendered > 3,
        "expanded long reasoning must wrap to >3 rows at 30 cols, got {rendered}"
    );
}

// The queue strip surfaces the Ctrl+G manager hint whenever the queue is
// non-empty — not only on the "+N more" overflow line. A 1- or 2-item queue
// must still show the entry so the user can edit/delete without discovering
// the key by accident.
#[test]
fn test_queue_strip_hint() {
    let mut app = working();
    app.pending
        .push(PendingItem::Message("first queued".into()));
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Ctrl+G to manage"),
        "1-item queue must hint Ctrl+G: {out}"
    );
    app.pending
        .push(PendingItem::Message("second queued".into()));
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Ctrl+G to manage"),
        "2-item queue must hint Ctrl+G: {out}"
    );
}

/// KNOWN BUG (deferred fix, characterization test). expanded_results is keyed
/// by bare call_id, so two result rows sharing a call_id (eager tool callers
/// reuse one call_id across distinct calls) collide: expanding ONE result
/// (Ctrl+O) also expands the OTHER. This pins the CURRENT buggy behavior —
/// the second result's long body renders fully (its tail "ZZ_SECRET_LAST"
/// shows) even though the user only expanded the first. When the fix lands
/// (key by call_id#ordinal, mirroring the fold-group fix), the second result
/// stays collapsed and this test FAILS — flip the assertion to
/// not-contains ZZ_SECRET_LAST at that point.
#[test]
fn test_expanded_results_callid_collide() {
    let mut app = working();
    // Two turns, each a bash call + result, sharing call_id c1 (eager reuse).
    // Result 1 body is short (shows fully either way). Result 2 body is 6
    // lines (> COLLAPSE_SHOW=3) ending in a distinctive tail.
    let r1 = "result one output".to_string();
    let r2 = "l1\nl2\nl3\nl4\nl5\nZZ_SECRET_LAST".to_string();
    let call = |cid: &str, brief: &str| TranscriptLine::Tool {
        name: "bash".to_string(),
        tool: "bash".to_string(),
        status: brief.to_string(),
        invocation: brief.to_string(),
        outcome: ToolOutcome::Success,
        call_id: cid.to_string(),
        body: String::new(),
        is_diff: false,
    };
    let result = |cid: &str, body: &str| TranscriptLine::Tool {
        name: "result".to_string(),
        tool: "result".to_string(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: cid.to_string(),
        body: body.to_string(),
        is_diff: false,
    };
    app.transcript = vec![
        TranscriptLine::User("turn one".into()),
        call("c1", "cmd1"),
        result("c1", &r1),
        TranscriptLine::Agent("mid".into()),
        call("c1", "cmd2"),
        result("c1", &r2),
    ];
    // Expand both fold groups so the result rows are visible (not hidden behind
    // a collapsed summary), then expand result 1's BODY via expanded_results.
    app.expanded_fold_groups.insert("c1#0".to_string());
    app.expanded_fold_groups.insert("c1#1".to_string());
    app.expanded_results.insert("c1".to_string());
    let out = render_text(&app, 80, 24);
    // BUG: result 2 (same call_id) renders EXPANDED, so its tail shows.
    assert!(
        out.contains("ZZ_SECRET_LAST"),
        "known bug: result 2 shares call_id c1 with the expanded result 1, so it renders expanded too:\n{out}"
    );
}

// Verbose mode count==render: the count path must match the draw path across
// all three verbose-sensitive sites in counts.rs. Mutation-tested: each of
// the three || self.verbose additions is pinned by a distinct corpus leg, so
// reverting any one branch reddens this test.
//   - :59 chip    -> a 30-line bash command: status truncates to 2 lines,
//                   render_verbose spans 30 (28-row desync without the fix).
//   - :49 result  -> a 10-line result body, past COLLAPSE_SHOW=3: collapsed
//                   shows 3 + ellipsis, expanded shows all 10 (6-row desync).
//   - :29 reasoning -> a 12-line ThoughtFor reasoning: collapsed is the
//                   header only, expanded wraps the reasoning to many rows.
// Three widths (40/80/120): narrow widths wrap reasoning + body to more rows
// than the header, the second desync source for :29.
#[test]
fn test_verbose_count_matches() {
    let mut app = working();
    let cmd: String = (0..30)
        .map(|i| format!("echo line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    // Past COLLAPSE_SHOW (3) so expanded vs collapsed differs in row count.
    let result_body: String = (0..10)
        .map(|i| format!("match_{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let reasoning: String = (0..12)
        .map(|i| format!("reasoning step {i} happens here"))
        .collect::<Vec<_>>()
        .join("\n");
    app.transcript = vec![
        TranscriptLine::User("search".into()),
        TranscriptLine::ThoughtFor {
            secs: 5,
            reasoning: Some(reasoning),
            tool_summary: None,
            turn_id: "t1".into(),
        },
        TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: crate::brief::tool_call_brief(
                "bash",
                &serde_json::json!({ "command": cmd.clone() }),
            ),
            invocation: cmd,
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            body: String::new(),
            is_diff: false,
        },
        TranscriptLine::Tool {
            name: "result".into(),
            tool: "bash".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            body: result_body,
            is_diff: false,
        },
        TranscriptLine::Agent("done".into()),
    ];
    app.verbose = true;
    for w in [40, 80, 120] {
        let out = render_text(&app, w, 50);
        let count = app.transcript_display_rows();
        let rendered = app.transcript_scroll.total.get();
        assert_eq!(
            count, rendered,
            "verbose desync at w={w}: count={count} rendered={rendered}\n{out}"
        );
    }
}
