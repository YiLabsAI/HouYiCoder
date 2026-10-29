//! The frames /sandbox and /rewind ask the host for, and the ones a restore
//! refuses to send.

use std::sync::mpsc;
use std::time::Duration;

use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointImage, CheckpointListView, CheckpointState, CheckpointView, UnreadableView,
};

use crate::state::{App, Pane};
use crate::test_harness::{TransportEvent, connected_app_with_events, wait_for_request};
use crate::ui::{CheckpointLevel, SandboxTab};

fn image(present: bool, hash: Option<&str>) -> CheckpointImage {
    CheckpointImage {
        present,
        hash: hash.map(str::to_string),
        bytes: hash.map(|_| 12),
    }
}

fn write(operation: &str, state: CheckpointState) -> CheckpointView {
    CheckpointView {
        operation: operation.into(),
        target: format!("src/{operation}.rs"),
        state,
        written_ms: Some(1_700_000_000_000),
        before: image(true, Some("before")),
        after: image(true, Some("after")),
    }
}

fn listing(writes: Vec<CheckpointView>) -> CheckpointListView {
    CheckpointListView {
        checkpoints: writes,
        unreadable: Vec::new(),
    }
}

/// An App with a connection, its pane open on the checkpoints tab holding the
/// host's answer already, so the tests below only have to act.
fn checkpoint_app(writes: Vec<CheckpointView>) -> (App, mpsc::Receiver<TransportEvent>) {
    let (mut app, events) = connected_app_with_events();
    app.ui.navigation.pane = Pane::Sandbox;
    app.ui.sandbox_pane.set_tab(SandboxTab::Checkpoints);
    let request = RequestId(70);
    app.ui.sandbox_pane.expect_list(request);
    app.apply_sandbox_checkpoints(request, listing(writes));
    (app, events)
}

/// Every restore request the app sends inside the window.
fn restore_frames(events: &mpsc::Receiver<TransportEvent>) -> Vec<RequestEnvelope> {
    let mut seen = Vec::new();
    while let Ok(TransportEvent::Frame(frame)) = events.recv_timeout(Duration::from_millis(150)) {
        if let Ok(ClientFrame::Request(envelope)) = serde_json::from_str(&frame)
            && matches!(envelope.payload, FrontendRequest::SandboxRestore { .. })
        {
            seen.push(envelope);
        }
    }
    seen
}

#[test]
fn test_sandbox_opens_boundaries() {
    let (mut app, events) = connected_app_with_events();
    app.run_command(SlashCommand::Sandbox);
    assert_eq!(app.ui.navigation.pane, Pane::Sandbox);
    assert_eq!(app.ui.sandbox_pane.tab.get(), SandboxTab::Sandbox);
    wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::SandboxBoundaries)
    });
}

#[test]
fn test_rewind_opens_checkpoints() {
    let (mut app, events) = connected_app_with_events();
    app.run_command(SlashCommand::Rewind);
    assert_eq!(app.ui.navigation.pane, Pane::Sandbox);
    assert_eq!(
        app.ui.sandbox_pane.tab.get(),
        SandboxTab::Checkpoints,
        "a bare /rewind opens the recorded writes rather than rewinding"
    );
    wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::SandboxCheckpoints)
    });
}

#[test]
fn test_switch_reads_other_tab() {
    let (mut app, events) = connected_app_with_events();
    app.run_command(SlashCommand::Sandbox);
    app.switch_sandbox_tab(1);
    assert_eq!(app.ui.sandbox_pane.tab.get(), SandboxTab::Checkpoints);
    wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::SandboxCheckpoints)
    });
}

#[test]
fn test_confirm_sends_restore() {
    let (mut app, events) = checkpoint_app(vec![write("cp-1", CheckpointState::Applied)]);
    app.begin_checkpoint_restore();
    assert_eq!(app.ui.sandbox_pane.level.get(), CheckpointLevel::Restore);
    app.confirm_checkpoint_restore();
    let envelope = wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::SandboxRestore { .. })
    });
    let FrontendRequest::SandboxRestore { operation } = envelope.payload else {
        panic!("the wait returned another request");
    };
    assert_eq!(
        operation, "cp-1",
        "the row under the cursor names the write"
    );
}

#[test]
fn test_confirm_waits_for_answer() {
    let (mut app, events) = checkpoint_app(vec![write("cp-1", CheckpointState::Applied)]);
    app.begin_checkpoint_restore();
    app.confirm_checkpoint_restore();
    app.confirm_checkpoint_restore();
    assert_eq!(
        restore_frames(&events).len(),
        1,
        "a restore already with the host stays the one request"
    );
}

#[test]
fn test_unverified_row_refuses_restore() {
    let (mut app, _events) = checkpoint_app(vec![write("cp-1", CheckpointState::Unverified)]);
    app.begin_checkpoint_restore();
    assert_eq!(
        app.ui.sandbox_pane.level.get(),
        CheckpointLevel::List,
        "no confirmation opens for a write whose earlier bytes were never kept"
    );
    let notice = app.ui.sandbox_pane.notice().unwrap_or_default();
    assert!(notice.contains("never kept"), "{notice}");
    assert!(
        notice.contains("src/cp-1.rs"),
        "the notice names the file, not the name the record is stored under: {notice}"
    );
}

/// A row the host could not judge answers the restore key with the reason it
/// was not judged, so the key the footer offers never does nothing.
#[test]
fn test_unreadable_row_says_why() {
    let (mut app, _events) = connected_app_with_events();
    app.ui.navigation.pane = Pane::Sandbox;
    app.ui.sandbox_pane.set_tab(SandboxTab::Checkpoints);
    let request = RequestId(71);
    app.ui.sandbox_pane.expect_list(request);
    app.apply_sandbox_checkpoints(
        request,
        CheckpointListView {
            checkpoints: vec![write("cp-1", CheckpointState::Applied)],
            unreadable: vec![UnreadableView {
                operation: Some("cp-9".into()),
                reason: "the record names no target".into(),
            }],
        },
    );
    app.ui.sandbox_pane.cursor.set(1);
    app.begin_checkpoint_restore();
    assert_eq!(
        app.ui.sandbox_pane.level.get(),
        CheckpointLevel::List,
        "no confirmation opens for a record the host could not judge"
    );
    let notice = app.ui.sandbox_pane.notice().unwrap_or_default();
    assert!(notice.contains("the record names no target"), "{notice}");
}

/// Opening a row's detail asks the host for that row's two sides, so the
/// comparison is read on demand rather than with the whole listing.
#[test]
fn test_detail_requests_sides() {
    let (mut app, events) = checkpoint_app(vec![write("cp-1", CheckpointState::Applied)]);
    app.open_checkpoint_detail();
    let envelope = wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::SandboxTexts { .. })
    });
    let FrontendRequest::SandboxTexts { operation } = envelope.payload else {
        panic!("the wait returned another request");
    };
    assert_eq!(operation, "cp-1", "the opened row names the write");
}

/// A record the host could not judge has no sides to read, so its detail asks
/// for nothing.
#[test]
fn test_unreadable_detail_reads_nothing() {
    let (mut app, _events) = connected_app_with_events();
    app.ui.navigation.pane = Pane::Sandbox;
    app.ui.sandbox_pane.set_tab(SandboxTab::Checkpoints);
    let request = RequestId(72);
    app.ui.sandbox_pane.expect_list(request);
    app.apply_sandbox_checkpoints(
        request,
        CheckpointListView {
            checkpoints: Vec::new(),
            unreadable: vec![UnreadableView {
                operation: None,
                reason: "the record names no target".into(),
            }],
        },
    );
    app.open_checkpoint_detail();
    assert_eq!(app.ui.sandbox_pane.level.get(), CheckpointLevel::Detail);
    assert!(
        app.ui.sandbox_pane.texts_request().is_none(),
        "an unreadable record is its own reason; there is nothing to read"
    );
}

#[test]
fn test_back_walks_levels() {
    let (mut app, _events) = checkpoint_app(vec![write("cp-1", CheckpointState::Applied)]);
    app.open_checkpoint_detail();
    assert_eq!(app.ui.sandbox_pane.level.get(), CheckpointLevel::Detail);
    app.sandbox_back();
    assert_eq!(app.ui.sandbox_pane.level.get(), CheckpointLevel::List);
    assert_eq!(app.ui.navigation.pane, Pane::Sandbox);
    app.sandbox_back();
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc closes the pane only at the listing"
    );
}
