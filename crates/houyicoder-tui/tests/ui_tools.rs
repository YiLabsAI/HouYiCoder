//! Real-binary PTY tests for tool-call driving + tool-result rendering. The
//! stub normally returns only plain text, so PTY could never reach the
//! interaction layer (permission cards, tool-result rendering, transcript
//! fold). These tests use HOUYICODER_STUB_SCRIPT to emit a scripted ToolCall
//! then plain text, so the real binary executes a real tool (read / glob /
//! edit) and the transcript renders the real result — the foundation for the
//! permission-flow + rendering-fidelity suites.
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_tools -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working_with_script};

/// A two-response script: the first carries a read ToolCall (so the runner
/// executes the real read tool on the workspace manifest), the second is plain
/// text so the run ends cleanly (a stateless stub would re-emit the ToolCall
/// every call and loop to max_turns). The path is relative to the binary's cwd
/// (the workspace root), so confine_path resolves it inside the workspace.
const READ_CARGO_TOML_SCRIPT: &str = r#"[
  [{"type":"ToolCall","id":"c1","name":"read","input":{"path":"Cargo.toml"}}],
  [{"type":"Text","text":"done"}]
]"#;

/// Foundation probe: a scripted read ToolCall drives the real read tool through
/// the binary. In Auto mode (the local-mode default) read auto-approves, so the
/// tool runs and the transcript renders the read result; the second response
/// ("done") ends the run. Asserts the tool executed + the run completed (the
/// script provider + tool execution + transcript render chain is wired) AND
/// that the default stub fallback did NOT fire (no "stub mode" reply). This is
/// the enabling test — the permission-flow + diff-rendering suites build on it.
///
/// Surfaced gap (noted, not fixed here): the read chip renders the server's
/// title ("Read 1 file"), not the path argument ("Cargo.toml") the way an
/// Update(path) "Added N lines" header does. The path-in-chip
/// fidelity is a separate rendering-alignment task once the spec is
/// pinned.
#[test]
#[ignore]
fn test_scripted_read_renders_chip() {
    let mut s = session_on_working_with_script(READ_CARGO_TOML_SCRIPT);
    s.send_str("read the manifest");
    s.send_key(&Key::Enter);
    // The read tool ran (its result summary renders) + the run ended ("done").
    s.wait_for("done", RENDER_TIMEOUT);
    assert!(
        s.output().contains("Read"),
        "the read tool result should render:\n{}",
        s.output()
    );
    // The script provider fired (not the default stub fallback).
    assert!(
        !s.output().contains("stub mode: no api key set"),
        "the scripted provider should drive the run, not the default stub:\n{}",
        s.output()
    );
}
