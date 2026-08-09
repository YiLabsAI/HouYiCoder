//! Tests for the artifact closed loop: load a real file via /artifact, enter an
//! edit mode (c/o/d/i) to propose a change, approve/reject the proposal, and
//! replay the applied change. Editing and review occur in one surface, with no
//! external editor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::frontend::SlashCommand;

use crate::composition;
use crate::keys::handle_working;
use crate::state::{App, Pane, Screen, ViewportMode};
use crate::test_support::render_text;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn working_app() -> App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}

/// Absolute path to this crate's lib.rs, so the load test does not depend on
/// the test runner CWD.
fn crate_src_path() -> String {
    format!("{}/src/lib.rs", env!("CARGO_MANIFEST_DIR"))
}

/// Type a string into the working input (char by char).
fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        handle_working(app, key(KeyCode::Char(c)));
    }
}

#[test]
fn test_artifact_command_loads_real() {
    let mut app = working_app();
    app.input.set(format!("/artifact {}", crate_src_path()));
    app.submit_input();
    assert_eq!(app.pane, Pane::Artifact);
    assert!(app.artifact.current_lines().len() > 1);
    let out = render_text(&app, 100, 28);
    println!("--- artifact pane (100x28) ---\n{out}\n--- end ---");
    assert!(out.contains("artifact"), "title missing:\n{out}");
    assert!(out.contains("content"), "content column missing:\n{out}");
    assert!(out.contains("review"), "review column missing:\n{out}");
}

#[test]
fn test_artifact_load_missing() {
    let mut app = working_app();
    app.input.set("/artifact /no/such/path.md".to_string());
    app.submit_input();
    assert_eq!(app.pane, Pane::Artifact);
    // The canned stub keeps the pane usable when the file cannot be read.
    assert!(app.artifact.current_lines().len() > 1);
}

#[test]
fn test_artifact_palette_fallback() {
    // /artifact is a string-only scaffold command (not in the palette). Typing
    // "artifact" in the palette filter finds no match; Enter falls back to
    // submitting the raw query so the command still runs.
    let mut app = working_app();
    app.open_palette();
    for c in "artifact".chars() {
        handle_working(&mut app, key(KeyCode::Char(c)));
    }
    assert_eq!(app.palette.query, "artifact");
    assert!(
        app.selected_command().is_none(),
        "/artifact is intentionally not in the palette (scaffold command)"
    );
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(app.pane, Pane::Artifact);
}

#[test]
fn test_artifact_replace_produces_pending() {
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    let focus = app.artifact.focus();
    // c enters Replace edit mode; typing + Enter proposes replacing the line.
    handle_working(&mut app, key(KeyCode::Char('c')));
    type_text(&mut app, "replacement line");
    handle_working(&mut app, key(KeyCode::Enter));
    let proposal = app
        .artifact
        .pending_proposal()
        .expect("replace produces a proposal");
    assert_eq!(proposal.proposed, vec!["replacement line".to_string()]);
    assert_eq!(proposal.line_start, focus);
    assert_eq!(
        proposal.original,
        vec![app.artifact.current_lines()[focus].clone()]
    );
}

#[test]
fn test_nl_note_skips_proposal() {
    // Natural-language mode attaches the text as an annotation and asks the
    // proposer. The stub cannot interpret NL without an LLM, so no proposal is
    // produced; the user is directed to the direct edit keys.
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Char('i')));
    type_text(&mut app, "rewrite this to be clearer");
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(app.artifact.pending_proposal().is_none());
    assert_eq!(app.artifact.annotation_count(), 1);
    assert!(matches!(app.transcript.last(),
        Some(crate::state::TranscriptLine::System(s)) if s.contains("natural-language")));
}

#[test]
fn test_artifact_approve_applies_edit() {
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    let focus = app.artifact.focus();
    handle_working(&mut app, key(KeyCode::Char('c')));
    type_text(&mut app, "new content");
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(app.artifact.pending_proposal().is_some());
    // Empty input + pending proposal: a applies the proposed edit.
    handle_working(&mut app, key(KeyCode::Char('a')));
    assert!(app.artifact.pending_proposal().is_none());
    assert_eq!(app.artifact.applied_count(), 1);
    assert_eq!(app.artifact.current_lines()[focus], "new content");
}

#[test]
fn test_artifact_reject_drops_proposal() {
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Char('c')));
    type_text(&mut app, "rejected text");
    handle_working(&mut app, key(KeyCode::Enter));
    let focus = app.artifact.focus();
    let before = app.artifact.current_lines()[focus].clone();
    handle_working(&mut app, key(KeyCode::Char('r')));
    assert!(app.artifact.pending_proposal().is_none());
    assert_eq!(app.artifact.applied_count(), 0);
    assert_eq!(app.artifact.current_lines()[focus], before);
}

#[test]
fn test_artifact_up_down_moves() {
    let mut app = working_app();
    app.pane = Pane::Artifact;
    let start = app.artifact.focus();
    handle_working(&mut app, key(KeyCode::Down));
    assert_ne!(app.artifact.focus(), start);
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.artifact.focus(), start);
}

#[test]
fn test_empty_enter_adds_nothing() {
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    let before = app.artifact.annotation_count();
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(app.artifact.annotation_count(), before);
}

#[test]
fn test_proposal_renders() {
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Char('c')));
    type_text(&mut app, "rendered");
    handle_working(&mut app, key(KeyCode::Enter));
    let out = render_text(&app, 100, 28);
    assert!(
        out.contains("proposed edit"),
        "pending proposal missing from review column:\n{out}"
    );
    assert!(
        out.contains("before:") && out.contains("after:"),
        "diff missing from review column:\n{out}"
    );
}

#[test]
fn test_artifact_applied_marker_render() {
    // After approving a replace, the applied line shows the ok marker in the
    // content column so the user can see which lines were reviewed and applied.
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Char('c')));
    type_text(&mut app, "applied line");
    handle_working(&mut app, key(KeyCode::Enter));
    handle_working(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.artifact.applied_count(), 1);
    let out = render_text(&app, 100, 28);
    assert!(
        out.contains("ok"),
        "applied-line ok marker missing from content:\n{out}"
    );
}

#[test]
fn test_artifact_insert_applies() {
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    let focus = app.artifact.focus();
    let before_len = app.artifact.current_lines().len();
    handle_working(&mut app, key(KeyCode::Char('o')));
    type_text(&mut app, "inserted line");
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(app.artifact.pending_proposal().is_some());
    handle_working(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.artifact.applied_count(), 1);
    assert_eq!(
        app.artifact.current_lines().len(),
        before_len + 1,
        "insert grows the document"
    );
    assert_eq!(
        app.artifact.current_lines()[focus + 1],
        "inserted line",
        "inserted line lands after the focused line"
    );
}

#[test]
fn test_artifact_delete_applies() {
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    let focus = app.artifact.focus();
    let removed = app.artifact.current_lines()[focus].clone();
    let before_len = app.artifact.current_lines().len();
    // d proposes an immediate delete of the focused line (no edit mode).
    handle_working(&mut app, key(KeyCode::Char('d')));
    assert!(app.artifact.pending_proposal().is_some());
    handle_working(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.artifact.applied_count(), 1);
    assert_eq!(
        app.artifact.current_lines().len(),
        before_len - 1,
        "delete shrinks the document"
    );
    assert!(
        !app.artifact.current_lines().contains(&removed),
        "deleted line is gone"
    );
}

#[test]
fn test_artifact_save_command_writes() {
    let path = format!(
        "{}/houyicoder_artifact_save_test.md",
        env!("CARGO_MANIFEST_DIR")
    );
    drop(std::fs::remove_file(&path));
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Char('c')));
    type_text(&mut app, "saved content");
    handle_working(&mut app, key(KeyCode::Enter));
    handle_working(&mut app, key(KeyCode::Char('a')));
    app.input.set(format!("/artifact-save {path}"));
    app.submit_input();
    let reloaded = crate::state::ArtifactSession::load(&path).expect("saved file loads back");
    assert!(
        reloaded
            .current_lines()
            .contains(&"saved content".to_string()),
        "applied edit persisted to disk"
    );
    drop(std::fs::remove_file(&path));
}

#[test]
fn test_artifact_page_down_moves() {
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    let start = app.artifact.focus();
    handle_working(&mut app, key(KeyCode::PageDown));
    assert_ne!(app.artifact.focus(), start);
    assert!(
        app.artifact.focus() >= start,
        "page down never moves backward"
    );
}

#[test]
fn test_artifact_esc_exits() {
    // Esc with empty input in Normal mode exits the artifact pane back to the
    // main transcript view.
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.pane, Pane::Transcript);
}

#[test]
fn test_artifact_esc_mid_typing() {
    // Mid-typing Esc in Normal mode is a no-op so typed text is not lost.
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.set("some text".to_string());
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.pane, Pane::Artifact);
}

#[test]
fn test_artifact_esc_cancels_edit() {
    // Esc in an edit mode cancels to Normal without exiting the pane.
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Char('c')));
    type_text(&mut app, "draft");
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.artifact.mode().is_normal());
    assert_eq!(app.pane, Pane::Artifact);
    assert!(app.artifact.pending_proposal().is_none());
    assert!(app.input.is_empty());
}

#[test]
fn test_normal_text_dropped() {
    // In Normal mode, plain (non-slash) text + Enter is a no-op: the input box
    // is for slash commands; edits start with c/o/d/i. The text is dropped.
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    let before = app.artifact.annotation_count();
    type_text(&mut app, "just typing");
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(app.artifact.annotation_count(), before);
    assert!(app.artifact.pending_proposal().is_none());
    assert!(app.input.is_empty());
}

#[test]
fn test_artifact_edit_mode_suppresses() {
    // In an edit mode, single-char shortcuts must not fire: q types into the
    // edit (no quit), and Tab does not cycle the pane (the mode is not
    // orphaned on a non-Artifact pane where Esc could no longer cancel it).
    let mut app = working_app();
    app.pane = Pane::Artifact;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Char('c')));
    assert!(!app.artifact.mode().is_normal());
    handle_working(&mut app, key(KeyCode::Char('q')));
    assert!(!app.quit, "q must not quit during an edit");
    assert_eq!(app.input.value(), "q");
    handle_working(&mut app, key(KeyCode::Tab));
    assert_eq!(app.pane, Pane::Artifact, "Tab must not flee the edit mode");
    assert!(
        !app.artifact.mode().is_normal(),
        "edit mode unchanged by Tab"
    );
}

#[test]
fn test_artifact_command_working_mode() {
    // /artifact must open in Working mode even from Focus, because artifact
    // editing needs the input box that Focus mode hides.
    let mut app = working_app();
    app.run_command(SlashCommand::Implement);
    assert_eq!(app.viewport, ViewportMode::Focus);
    app.input.set(format!("/artifact {}", crate_src_path()));
    app.submit_input();
    assert_eq!(app.pane, Pane::Artifact);
    assert_eq!(
        app.viewport,
        ViewportMode::Working,
        "artifact pane must open in Working mode so the input box shows"
    );
}
