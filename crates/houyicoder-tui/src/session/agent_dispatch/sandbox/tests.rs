//! Where the host's sandbox answers land: the pane that asked, or the
//! transcript when nothing waits for them.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointImage, CheckpointListView, CheckpointRestoreView, CheckpointSide, CheckpointState,
    CheckpointTextView, CheckpointView, FenceState, RestoreView, SandboxBoundariesView,
};

use crate::agent_message::{ServerResponse, SessionMessage};
use crate::records::TranscriptLine;
use crate::state::{App, Pane};
use crate::test_harness::{connected_app_with_events, render_text};
use crate::ui::{CheckpointLevel, SandboxTab};

fn boundaries() -> SandboxBoundariesView {
    SandboxBoundariesView {
        fence: FenceState::Enforced,
        workspace_root: "/w".into(),
        writable_roots: vec!["/w".into()],
        scratch_root: None,
    }
}

fn restored(outcome: RestoreView) -> CheckpointRestoreView {
    CheckpointRestoreView {
        operation: "cp-1".into(),
        outcome,
    }
}

fn listing() -> CheckpointListView {
    CheckpointListView {
        checkpoints: vec![CheckpointView {
            operation: "cp-1".into(),
            target: "src/cp-1.rs".into(),
            state: CheckpointState::Applied,
            written_ms: Some(1_700_000_000_000),
            before: CheckpointImage {
                present: true,
                hash: None,
                bytes: Some(4),
            },
            after: CheckpointImage {
                present: true,
                hash: None,
                bytes: Some(5),
            },
        }],
        unreadable: Vec::new(),
    }
}

fn texts() -> CheckpointTextView {
    CheckpointTextView {
        operation: "cp-1".into(),
        target: "src/cp-1.rs".into(),
        before: CheckpointSide::Text {
            text: "old".into(),
            truncated: false,
        },
        after: CheckpointSide::Text {
            text: "new".into(),
            truncated: false,
        },
    }
}

fn loaded(app: &mut App) {
    let request = RequestId(89);
    app.ui.sandbox_pane.expect_list(request);
    app.apply_sandbox_checkpoints(request, listing());
}

fn answer(app: &mut App, request: RequestId, response: ServerResponse) {
    app.handle_agent_message(SessionMessage::Response { request, response });
}

fn pane_app() -> App {
    let (mut app, _events) = connected_app_with_events();
    app.ui.navigation.pane = Pane::Sandbox;
    app.ui.sandbox_pane.set_tab(SandboxTab::Checkpoints);
    app
}

fn notices(app: &App) -> Vec<String> {
    app.sessions
        .current
        .transcript
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::System(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn test_boundaries_answer_lands() {
    let mut app = pane_app();
    app.ui.sandbox_pane.set_tab(SandboxTab::Sandbox);
    let request = RequestId(80);
    app.ui.sandbox_pane.expect_boundaries(request);
    answer(
        &mut app,
        request,
        ServerResponse::SandboxBoundaries { view: boundaries() },
    );
    let read = app.ui.sandbox_pane.boundaries().expect("the answer landed");
    assert_eq!(read.fence, FenceState::Enforced);
    assert!(
        app.ui.sandbox_pane.boundaries_request().is_none(),
        "the pane stops waiting once the answer lands"
    );
}

#[test]
fn test_stale_boundaries_dropped() {
    let mut app = pane_app();
    app.ui.sandbox_pane.set_tab(SandboxTab::Sandbox);
    app.ui.sandbox_pane.expect_boundaries(RequestId(80));
    answer(
        &mut app,
        RequestId(81),
        ServerResponse::SandboxBoundaries { view: boundaries() },
    );
    assert!(
        app.ui.sandbox_pane.boundaries().is_none(),
        "an answer to another request does not fill the pane"
    );
}

/// The confirmation closes back to where it was opened, and the outcome goes
/// to the transcript, where it survives the pane closing. The sentence names
/// the file the user restored, not the name the record is stored under.
#[test]
fn test_restore_returns_level() {
    let mut app = pane_app();
    loaded(&mut app);
    let request = RequestId(82);
    app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
    app.ui.sandbox_pane.begin_confirm();
    app.ui.sandbox_pane.begin_restore(request);
    answer(
        &mut app,
        request,
        ServerResponse::SandboxRestored {
            view: restored(RestoreView::Restored),
        },
    );
    assert_eq!(
        app.ui.sandbox_pane.level.get(),
        CheckpointLevel::Detail,
        "the confirmation closes back to where it was opened"
    );
    let lines = notices(&app);
    assert!(
        lines.iter().any(|l| l.contains("restored src/cp-1.rs")),
        "the outcome reaches the transcript: {lines:?}"
    );
    assert!(
        app.ui.sandbox_pane.notice().is_none(),
        "the same sentence is not also kept on the pane"
    );
}

/// A restore that returns to the detail reads the two sides again, so the
/// comparison shows what the target holds now.
#[test]
fn test_restore_rereads_sides() {
    let mut app = pane_app();
    loaded(&mut app);
    let request = RequestId(90);
    app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
    app.ui.sandbox_pane.begin_confirm();
    app.ui.sandbox_pane.begin_restore(request);
    answer(
        &mut app,
        request,
        ServerResponse::SandboxRestored {
            view: restored(RestoreView::Restored),
        },
    );
    assert!(
        app.ui.sandbox_pane.reading_texts(),
        "the detail asks for both sides again"
    );
}

#[test]
fn test_stale_restore_reaches_transcript() {
    let mut app = pane_app();
    answer(
        &mut app,
        RequestId(83),
        ServerResponse::SandboxRestored {
            view: restored(RestoreView::Unbacked),
        },
    );
    let lines = notices(&app);
    assert!(
        lines.iter().any(|l| l.contains("were never kept")),
        "a restore nobody waits on still reaches the reader: {lines:?}"
    );
    assert!(
        lines.iter().all(|l| !l.contains("cp-1")),
        "the sentence names no name the record is stored under: {lines:?}"
    );
}

/// A text answer lands on the detail that asked for it, and an answer to a
/// read the pane no longer waits on is dropped.
#[test]
fn test_texts_answer_lands() {
    let mut app = pane_app();
    let request = RequestId(86);
    app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
    app.ui.sandbox_pane.expect_texts(request);
    answer(
        &mut app,
        request,
        ServerResponse::SandboxTexts { view: texts() },
    );
    assert!(app.ui.sandbox_pane.texts().is_some(), "the answer landed");
    assert!(
        app.ui.sandbox_pane.texts_request().is_none(),
        "the pane stops waiting once the answer lands"
    );

    app.ui.sandbox_pane.forget_texts();
    app.ui.sandbox_pane.expect_texts(RequestId(87));
    answer(
        &mut app,
        RequestId(88),
        ServerResponse::SandboxTexts { view: texts() },
    );
    assert!(
        app.ui.sandbox_pane.texts().is_none(),
        "an answer to another request does not fill the detail"
    );
}

/// A text read that failed states why on the detail rather than leaving the
/// comparison looking like a file with no bytes on either side.
#[test]
fn test_texts_failure_notice() {
    let mut app = pane_app();
    let request = RequestId(91);
    app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
    app.ui.sandbox_pane.expect_texts(request);
    answer(
        &mut app,
        request,
        ServerResponse::Error {
            message: "the record is gone".into(),
        },
    );
    assert!(app.ui.sandbox_pane.texts_request().is_none());
    let out = render_text(&app, 100, 30);
    assert!(out.contains("cannot read the recorded sides"), "{out}");
    assert!(out.contains("the record is gone"), "{out}");
}

/// A failed read names its reason on the screen the user is looking at, not
/// only in the pane's state.
#[test]
fn test_read_failure_notice() {
    let mut app = pane_app();
    let request = RequestId(84);
    app.ui.sandbox_pane.expect_list(request);
    answer(
        &mut app,
        request,
        ServerResponse::Error {
            message: "no session store".into(),
        },
    );
    assert!(app.ui.sandbox_pane.list_request().is_none());
    let out = render_text(&app, 100, 30);
    assert!(out.contains("cannot list the checkpoints"), "{out}");
    assert!(out.contains("no session store"), "{out}");
}

#[test]
fn test_empty_list_settles() {
    let mut app = pane_app();
    let request = RequestId(85);
    app.ui.sandbox_pane.expect_list(request);
    app.ui.sandbox_pane.move_cursor(5, 3);
    answer(
        &mut app,
        request,
        ServerResponse::SandboxCheckpoints {
            view: CheckpointListView {
                checkpoints: Vec::new(),
                unreadable: Vec::new(),
            },
        },
    );
    assert_eq!(
        app.ui.sandbox_pane.cursor.get(),
        0,
        "an empty listing leaves no row under the cursor"
    );
    assert_eq!(
        app.ui.sandbox_pane.tab.get(),
        SandboxTab::Checkpoints,
        "the tab the answer was asked for stays open"
    );
}
