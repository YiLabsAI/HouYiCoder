//! Where the host's sandbox answers land: the pane that asked, or the
//! transcript when nothing waits for them.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointListView, CheckpointRestoreView, FenceState, RestoreView, SandboxBoundariesView,
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
    }
}

fn restored(outcome: RestoreView) -> CheckpointRestoreView {
    CheckpointRestoreView {
        operation: "cp-1".into(),
        outcome,
    }
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
    app.ui.sandbox_pane.set_tab(SandboxTab::Boundaries);
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
    app.ui.sandbox_pane.set_tab(SandboxTab::Boundaries);
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
/// to the transcript, where it survives the pane closing.
#[test]
fn test_restore_returns_level() {
    let mut app = pane_app();
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
        lines.iter().any(|l| l.contains("cp-1")),
        "the outcome reaches the transcript: {lines:?}"
    );
    assert!(
        app.ui.sandbox_pane.notice().is_none(),
        "the same sentence is not also kept on the pane"
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
        lines.iter().any(|l| l.contains("cp-1")),
        "a restore nobody waits on still reaches the reader: {lines:?}"
    );
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
