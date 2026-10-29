//! The host's answers to the /worktrees pane verbs, as the pane shows them.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::worktree::WorktreeEnterView;
use houyicoder_protocol::frontend::worktree::WorktreeRemoveOutcome;

use crate::composition::WorktreeEntry;
use crate::composition::app;
use crate::records::TranscriptLine;
use crate::state::App;
use crate::state::Pane;
use crate::state::Screen;
use crate::test_harness::render_text;
use crate::test_harness::worktree_path;
use crate::test_harness::worktree_preview;
use crate::ui::WorktreeLevel;

fn pane_app() -> App {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Worktree;
    app.sessions.current.capabilities.worktree.entries = vec![WorktreeEntry {
        path: worktree_path("alpha"),
        head: "abcdef0".into(),
        branch: "worktree-alpha".into(),
        ..Default::default()
    }];
    app
}

fn waiting_on(request: RequestId, level: WorktreeLevel) -> App {
    let app = pane_app();
    {
        let mut draft = app.ui.worktree_pane.draft.borrow_mut();
        draft.begin(&worktree_path("alpha"), WorktreeLevel::List, None);
        draft.expect_preview(request);
    }
    app.ui.worktree_pane.level.set(level);
    app
}

fn holding_preview(level: WorktreeLevel) -> App {
    let app = pane_app();
    {
        let mut draft = app.ui.worktree_pane.draft.borrow_mut();
        draft.begin(&worktree_path("alpha"), WorktreeLevel::List, None);
        draft.accept(worktree_preview("alpha"));
    }
    app.ui.worktree_pane.level.set(level);
    app
}

/// A pane whose confirmed removal is with the host under this request.
fn removing(request: RequestId) -> App {
    let app = holding_preview(WorktreeLevel::Confirm);
    app.ui
        .worktree_pane
        .draft
        .borrow_mut()
        .take_submission(request)
        .expect("a free worktree submits");
    app
}

/// A pane whose enter is with the host under this request.
fn entering(request: RequestId) -> App {
    let app = holding_preview(WorktreeLevel::Detail);
    app.ui
        .worktree_pane
        .draft
        .borrow_mut()
        .expect_enter(request);
    app
}

fn render(app: &App) -> String {
    render_text(app, 80, 24)
}

fn recorded(app: &App, needle: &str) -> bool {
    app.sessions
        .current
        .transcript
        .iter()
        .any(|line| matches!(line, TranscriptLine::System(text) if text.contains(needle)))
}

/// Whether the pane still offers the tree these cases start from.
fn offers_alpha(app: &App) -> bool {
    app.sessions
        .current
        .capabilities
        .worktree
        .entries
        .iter()
        .any(|entry| entry.path == worktree_path("alpha"))
}

fn enter_view() -> WorktreeEnterView {
    WorktreeEnterView {
        path: worktree_path("alpha"),
        branch: "worktree-alpha".to_string(),
    }
}

/// The answer to the request the pane sent fills the detail it is showing.
#[test]
fn test_preview_answer_fills_draft() {
    let mut app = waiting_on(RequestId(3), WorktreeLevel::Detail);
    app.apply_worktree_preview(RequestId(3), worktree_preview("alpha"));
    let out = render(&app);
    assert!(
        out.contains("Changes:  2 tracked dirty · 1 untracked"),
        "{out}"
    );
    assert!(!out.contains("reading from the host"), "{out}");
}

/// An answer to a request the pane no longer waits on is dropped, so a reply
/// that arrives late cannot replace a newer read.
#[test]
fn test_late_preview_answer_dropped() {
    let mut app = waiting_on(RequestId(3), WorktreeLevel::Detail);
    app.apply_worktree_preview(RequestId(9), worktree_preview("alpha"));
    let out = render(&app);
    assert!(out.contains("reading from the host"), "{out}");
    assert!(!out.contains("tracked dirty"), "{out}");
}

/// Entering names where the session now works, points the session there, and
/// returns the pane to the list so the next action starts from the rows.
#[test]
fn test_enter_answer_names_location() {
    let mut app = entering(RequestId(4));
    app.apply_worktree_entered(RequestId(4), enter_view());
    assert!(
        recorded(
            &app,
            "worktree: now working in /some/repo/alpha (worktree-alpha)"
        ),
        "the transcript names the new location"
    );
    assert_eq!(
        app.sessions.current.identity.working_dir,
        worktree_path("alpha")
    );
    assert_eq!(app.ui.worktree_pane.level.get(), WorktreeLevel::List);
}

/// An enter the pane has stopped waiting for still changed where the session
/// works, so the transcript records it and the pane stays where the user is.
#[test]
fn test_late_enter_answer_recorded() {
    let mut app = entering(RequestId(4));
    app.apply_worktree_entered(RequestId(9), enter_view());
    assert!(recorded(&app, "worktree: now working in"));
    assert_eq!(
        app.sessions.current.identity.working_dir,
        worktree_path("alpha")
    );
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::Detail,
        "the pane the user left open stays where it is"
    );
}

/// A finished removal empties the pane, says so in the transcript, and goes
/// back to the list.
#[test]
fn test_removal_answer_clears_pane() {
    let mut app = removing(RequestId(5));
    app.apply_worktree_removed(RequestId(5), WorktreeRemoveOutcome::Removed);
    assert!(recorded(&app, "worktree: removed"));
    assert_eq!(app.ui.worktree_pane.level.get(), WorktreeLevel::List);
    assert_eq!(app.ui.worktree_pane.draft.borrow().path(), None);
}

/// A stale answer removed nothing: the pane says so, reads the tree again,
/// and takes a submission again instead of leaving Enter dead.
#[test]
fn test_stale_removal_reopens_enter() {
    let mut app = removing(RequestId(5));
    app.apply_worktree_removed(RequestId(5), WorktreeRemoveOutcome::Stale);
    let out = render(&app);
    assert!(
        out.contains("the worktree changed since the preview; nothing was removed"),
        "{out}"
    );
    let mut draft = app.ui.worktree_pane.draft.borrow_mut();
    assert!(
        draft.preview().is_none(),
        "the facts the removal was decided on are dropped"
    );
    draft.accept(worktree_preview("alpha"));
    assert!(
        draft.take_submission(RequestId(6)).is_some(),
        "the next Enter submits against what the tree holds now"
    );
}

/// A removal answer that no longer matches the pane's request still reports
/// what the host did, and the pane keeps waiting on its own removal.
#[test]
fn test_unmatched_removal_answer_recorded() {
    let mut app = removing(RequestId(5));
    app.apply_worktree_removed(RequestId(6), WorktreeRemoveOutcome::Removed);
    assert!(recorded(&app, "worktree: removed"));
    assert_eq!(
        app.ui.worktree_pane.draft.borrow().request(),
        Some(RequestId(5)),
        "an answer to another request does not settle this removal"
    );
}

/// A removal whose directory went but whose branch survived names the branch
/// that is still there.
#[test]
fn test_incomplete_removal_names_survivor() {
    let mut app = removing(RequestId(5));
    app.apply_worktree_removed(
        RequestId(5),
        WorktreeRemoveOutcome::Incomplete {
            path: worktree_path("alpha"),
            branch: "worktree-alpha".to_string(),
            reason: "checked out elsewhere".to_string(),
        },
    );
    let out = render(&app);
    assert!(
        out.contains(
            "the directory is gone; branch worktree-alpha survives: checked out elsewhere"
        ),
        "{out}"
    );
    assert!(
        !offers_alpha(&app),
        "the rows are read again, so the list no longer offers the tree that is gone"
    );
}

/// A removal that left the directory behind says what stopped it, so the next
/// attempt starts from the fact rather than from scratch.
#[test]
fn test_pending_removal_names_cause() {
    let mut app = removing(RequestId(5));
    app.apply_worktree_removed(
        RequestId(5),
        WorktreeRemoveOutcome::Pending {
            path: worktree_path("alpha"),
            reason: "directory in use".to_string(),
        },
    );
    let out = render(&app);
    assert!(
        out.contains("the directory survives: directory in use"),
        "{out}"
    );
    assert!(
        !offers_alpha(&app),
        "the rows are read again, so the list reflects the removal that stopped"
    );
}

/// A refusal for the removal the pane is waiting on settles it and shows the
/// reason; a refusal for anything else is left to the caller.
#[test]
fn test_refusal_settles_removal() {
    let mut app = removing(RequestId(5));
    assert!(app.settle_worktree_failure(RequestId(5), "the tree is locked"));
    let out = render(&app);
    assert!(out.contains("removal refused: the tree is locked"), "{out}");
    assert!(!app.settle_worktree_failure(RequestId(6), "another request"));
}

/// A refusal for the preview the pane is waiting on stops the read and says
/// what the host answered.
#[test]
fn test_refusal_settles_preview() {
    let mut app = waiting_on(RequestId(7), WorktreeLevel::Confirm);
    assert!(app.settle_worktree_failure(RequestId(7), "no git repository here"));
    let out = render(&app);
    assert!(
        out.contains("cannot read the worktree: no git repository here"),
        "{out}"
    );
}

/// A detail whose read ended without a preview says why instead of waiting on
/// an answer that is no longer coming.
#[test]
fn test_detail_shows_refusal() {
    let mut app = waiting_on(RequestId(7), WorktreeLevel::Detail);
    assert!(app.settle_worktree_failure(RequestId(7), "no git repository here"));
    let out = render(&app);
    assert!(
        out.contains("cannot read the worktree: no git repository here"),
        "{out}"
    );
    assert!(!out.contains("reading from the host"), "{out}");
}

/// A refused enter reaches the transcript, because the user asked to move and
/// the session did not move.
#[test]
fn test_refusal_reaches_transcript() {
    let mut app = entering(RequestId(4));
    assert!(app.settle_worktree_failure(RequestId(4), "already in a worktree session"));
    assert!(recorded(
        &app,
        "worktree: cannot enter: already in a worktree session"
    ));
    assert_eq!(app.ui.worktree_pane.draft.borrow().enter_request(), None);
}
