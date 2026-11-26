//! /lsp pane rendering: the list rows with state and scope, the detail's
//! command and counts, and the key hints trimmed to what the selected row
//! accepts.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::lsp::{LspConfigScope, LspServerState, LspServerStatusView};

use crate::composition;
use crate::state::{App, Pane, Screen};
use crate::test_harness::render_text;

fn row(
    name: &str,
    state: LspServerState,
    scope: LspConfigScope,
    locked: bool,
) -> LspServerStatusView {
    LspServerStatusView {
        name: name.into(),
        scope,
        state,
        locked,
        command_line: "rust-analyzer --log-file /tmp/lsp.log".into(),
        extensions: vec!["rs".into()],
        open_documents: 3,
        diagnostic_count: 2,
        recent_stderr: vec!["a child wrote this".into()],
    }
}

fn pane_app(rows: Vec<LspServerStatusView>) -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Lsp;
    app.ui.lsp_pane.mark_pending(RequestId(1));
    app.ui.lsp_pane.apply_status(RequestId(1), rows);
    app
}

fn sample_rows() -> Vec<LspServerStatusView> {
    vec![
        row(
            "alpha",
            LspServerState::Running,
            LspConfigScope::User,
            false,
        ),
        row(
            "beta",
            LspServerState::Failed {
                error: "the server exited".into(),
            },
            LspConfigScope::Project,
            false,
        ),
    ]
}

/// The list names each server with its state, scope, and owned suffixes,
/// and the header counts the running ones.
#[test]
fn test_list_renders_rows() {
    let app = pane_app(sample_rows());
    let out = render_text(&app, 80, 24);
    assert!(out.contains("LSP"), "the header shows: {out}");
    assert!(
        out.contains("2 servers · 1 running"),
        "the count shows: {out}"
    );
    assert!(out.contains("alpha"), "the first row shows: {out}");
    assert!(out.contains("running"), "the state shows: {out}");
    assert!(out.contains("user"), "the scope shows: {out}");
    assert!(out.contains(".rs"), "the suffix shows: {out}");
    assert!(out.contains("failed · the server exited"), "{out}");
    assert!(out.contains("project"), "the second scope shows: {out}");
}

/// An empty list says so rather than rendering a bare header.
#[test]
fn test_empty_list_explains() {
    let app = pane_app(Vec::new());
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("no language servers configured"),
        "the empty state explains: {out}"
    );
}

/// The detail pins the command and the counts above the stderr tail, and
/// offers the action keys the row accepts.
#[test]
fn test_detail_renders_facts() {
    let mut app = pane_app(sample_rows());
    app.ui.lsp_pane.open_detail();
    let out = render_text(&app, 80, 30);
    assert!(out.contains("command  rust-analyzer"), "{out}");
    assert!(out.contains("documents  3"), "{out}");
    assert!(out.contains("diagnostics  2"), "{out}");
    assert!(out.contains("stderr (recent)"), "{out}");
    assert!(out.contains("restart"), "the hint offers restart: {out}");
    assert!(out.contains("toggle"), "the hint offers toggle: {out}");
}

/// Every lifecycle state and every scope renders its own label, so a state
/// the host reports never reads as another.
#[test]
fn test_state_labels_render() {
    let app = pane_app(vec![
        row("plain", LspServerState::Idle, LspConfigScope::User, false),
        row(
            "warming",
            LspServerState::Starting,
            LspConfigScope::Project,
            false,
        ),
        row(
            "busy",
            LspServerState::Indexing {
                message: Some("crates".into()),
            },
            LspConfigScope::Plugin,
            false,
        ),
        row(
            "quiet",
            LspServerState::Indexing { message: None },
            LspConfigScope::PluginCache,
            false,
        ),
        row(
            "off",
            LspServerState::Disabled {
                reason: "disabled by the user".into(),
            },
            LspConfigScope::User,
            false,
        ),
    ]);
    let out = render_text(&app, 100, 30);
    assert!(out.contains("idle"), "{out}");
    assert!(out.contains("starting"), "{out}");
    assert!(out.contains("indexing · crates"), "{out}");
    assert!(out.contains("project"), "{out}");
    assert!(out.contains("plugin"), "{out}");
    assert!(out.contains("plugin cache"), "{out}");
    assert!(out.contains("disabled · disabled by the user"), "{out}");
    assert!(
        out.contains("5 servers · 2 running"),
        "indexing counts as up: {out}"
    );
}

/// A locked row reads as held by the trust gate and trims the toggle hint,
/// so the pane never offers a key that cannot act.
#[test]
fn test_locked_row_hides_toggle() {
    let app = pane_app(vec![row(
        "gated",
        LspServerState::Disabled {
            reason: "the workspace is not trusted".into(),
        },
        LspConfigScope::Project,
        true,
    )]);
    let out = render_text(&app, 80, 24);
    assert!(out.contains("locked · untrusted workspace"), "{out}");
    assert!(
        !out.contains("toggle"),
        "the locked row offers no toggle: {out}"
    );
}
