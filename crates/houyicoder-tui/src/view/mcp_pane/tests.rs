//! /mcp pane rendering: the list rows with state and scope, the detail's
//! masked headers and tool labels, and the key hints trimmed to what the
//! selected row accepts.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::mcp::{
    McpConfigScope, McpConnectionState, McpServerDetailView, McpServerStatusView, McpToolSummary,
};

use crate::composition;
use crate::state::{App, Pane, Screen};
use crate::test_harness::render_text;

fn row(name: &str, state: McpConnectionState, scope: McpConfigScope) -> McpServerStatusView {
    McpServerStatusView {
        name: name.into(),
        scope,
        state,
        server_version: Some("1.4.2".into()),
        tool_count: 2,
        resource_count: 1,
        has_instructions: true,
        recent_stderr: vec!["a child wrote this".into()],
        detail: McpServerDetailView {
            endpoint: "npx -y some-server".into(),
            header_names: vec!["Authorization".into(), "X-Trace-Id".into()],
            tools: vec![
                McpToolSummary {
                    name: "read_file".into(),
                    read_only: true,
                    description: "reads one file".into(),
                },
                McpToolSummary {
                    name: "write_file".into(),
                    read_only: false,
                    description: "writes one file".into(),
                },
            ],
            instructions_preview: Some("handle the workspace carefully".into()),
            untrusted_lock: false,
        },
    }
}

fn pane_app(rows: Vec<McpServerStatusView>) -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Mcp;
    app.ui.mcp_pane.mark_pending(RequestId(1));
    app.ui.mcp_pane.apply_status(RequestId(1), rows);
    app
}

fn sample_rows() -> Vec<McpServerStatusView> {
    vec![
        row("alpha", McpConnectionState::Connected, McpConfigScope::User),
        row(
            "beta",
            McpConnectionState::Failed {
                error: "the port refused".into(),
            },
            McpConfigScope::Project,
        ),
    ]
}

/// The list names each server with its state and scope, and the header
/// counts the connected ones.
#[test]
fn test_list_renders_rows() {
    let app = pane_app(sample_rows());
    let out = render_text(&app, 80, 24);
    assert!(out.contains("MCP"), "the header shows: {out}");
    assert!(
        out.contains("2 servers · 1 connected"),
        "the count shows: {out}"
    );
    assert!(out.contains("alpha"), "the first row shows: {out}");
    assert!(out.contains("connected"), "the state shows: {out}");
    assert!(out.contains("user"), "the scope shows: {out}");
    assert!(
        out.contains("failed · the port refused"),
        "the failure names its cause: {out}"
    );
    assert!(out.contains("project"), "the second scope shows: {out}");
}

/// An empty list says so, and the hints offer no action keys that have no
/// row to act on.
#[test]
fn test_list_empty_message() {
    let app = pane_app(Vec::new());
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("no servers configured"),
        "the empty state shows: {out}"
    );
    assert!(
        !out.contains("r to reconnect"),
        "no row, no reconnect hint: {out}"
    );
}

/// The detail pins the identity and scope, and the body carries the
/// transport, the masked headers, the tools with their safety labels, the
/// instructions preview, and the stderr tail.
#[test]
fn test_detail_masks_headers() {
    let mut app = pane_app(sample_rows());
    app.ui.mcp_pane.open_detail();
    let out = render_text(&app, 80, 32);
    assert!(out.contains("alpha"), "the identity stays pinned: {out}");
    assert!(out.contains("server 1.4.2"), "the version shows: {out}");
    assert!(
        out.contains("transport  npx -y some-server"),
        "the endpoint shows: {out}"
    );
    assert!(
        out.contains("Authorization: ***"),
        "a header renders masked, never its value: {out}"
    );
    assert!(
        out.contains("X-Trace-Id: ***"),
        "each header gets its own row: {out}"
    );
    assert!(
        out.contains("read_file [read-only]"),
        "the safe tool carries its label: {out}"
    );
    assert!(
        out.contains("write_file [approval-required]"),
        "the unsafe tool carries its label: {out}"
    );
    assert!(
        out.contains("handle the workspace carefully"),
        "the instructions preview shows: {out}"
    );
    assert!(
        out.contains("a child wrote this"),
        "the stderr tail shows: {out}"
    );
}

/// The key hints follow the selected row: a connected row offers both
/// action keys, a locked one offers neither, and the detail swaps the
/// select hint for the scroll hint.
#[test]
fn test_hints_follow_selection() {
    let mut rows = sample_rows();
    rows[1].state = McpConnectionState::Disabled {
        reason: "the workspace is not trusted".into(),
    };
    rows[1].detail.untrusted_lock = true;
    let mut app = pane_app(rows);
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("r to reconnect"),
        "the connected row offers reconnect: {out}"
    );
    assert!(
        out.contains("t to toggle"),
        "the connected row offers toggle: {out}"
    );

    app.ui.mcp_pane.cursor = 1;
    let out = render_text(&app, 80, 24);
    assert!(
        !out.contains("r to reconnect"),
        "the locked row hides reconnect: {out}"
    );
    assert!(
        !out.contains("t to toggle"),
        "the locked row hides toggle: {out}"
    );
    assert!(
        out.contains("locked · untrusted workspace"),
        "the lock is visible: {out}"
    );

    app.ui.mcp_pane.cursor = 0;
    app.ui.mcp_pane.open_detail();
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Up/Down to scroll"),
        "the detail scrolls: {out}"
    );
    assert!(
        out.contains("Esc to back"),
        "the detail returns, not closes: {out}"
    );
}
