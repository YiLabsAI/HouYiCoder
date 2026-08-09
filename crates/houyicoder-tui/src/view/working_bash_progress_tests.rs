//! Bash chip progress render + dispatch tests, split from working_tests.rs +
//! agent_dispatch.rs so those files stay under the file-size gate.

use crate::composition;
use crate::records::{ToolOutcome, TranscriptLine};
use crate::run_control::AgentMessage;
use crate::state::{BashProgress, Screen};
use crate::test_support::render_text;
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::frontend::session_update::{
    SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};

fn tcall(cid: &str, name: &str, brief: &str, oc: ToolOutcome) -> TranscriptLine {
    TranscriptLine::Tool {
        name: name.to_string(),
        tool: name.to_string(),
        status: brief.to_string(),
        invocation: brief.to_string(),
        outcome: oc,
        call_id: cid.to_string(),
        body: String::new(),
        is_diff: false,
    }
}

/// A long-running bash call shows (Ns) on its chip after 2s so a
/// stalled-looking command is distinguishable from a stuck one (a
/// shell-progress-suffix). <2s does not show (fast commands stay
/// clean). When the backend streams stdout (lines Some), (Ns · M lines)
/// lands. Pins the render-time injection.
#[test]
fn test_bash_chip_shows_elapsed() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![
        TranscriptLine::User("hi".into()),
        tcall("c1", "bash", "npm install", ToolOutcome::Success),
    ];
    app.running_tools.insert("c1".into());
    // agent_busy keeps the group active so it stays expanded and the chip
    // renders as a Line, not collapsed to a summary. Without this the call
    // would fold and the chip never renders.
    app.agent_busy = true;
    // <2s: no suffix (fast commands stay clean).
    app.bash_progress.insert(
        "c1".into(),
        BashProgress {
            elapsed_secs: 1,
            lines: None,
        },
    );
    let out = render_text(&app, 80, 24);
    assert!(out.contains("Bash(npm install)"), "chip present: {out}");
    assert!(!out.contains("1s)"), "under-2s no suffix: {out}");
    // >=2s, no streaming (lines None): (Ns) suffix lands.
    app.bash_progress.insert(
        "c1".into(),
        BashProgress {
            elapsed_secs: 12,
            lines: None,
        },
    );
    let out = render_text(&app, 80, 24);
    assert!(out.contains("(12s)"), "elapsed suffix on chip: {out}");
    assert!(
        !out.contains("lines"),
        "no lines suffix when not streaming: {out}"
    );
    // lines Some (streaming): (Ns · M lines) lands.
    app.bash_progress.insert(
        "c1".into(),
        BashProgress {
            elapsed_secs: 12,
            lines: Some(14),
        },
    );
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("(12s · 14 lines)"),
        "elapsed + lines suffix on chip: {out}"
    );
}

// --- ToolProgress dispatch test (moved from agent_dispatch.rs) ---
// Inlined frame helpers (tool_call_frame / tool_done_frame) so this file is
// self-contained without importing the dispatch test module privates.

fn tool_call_frame(id: &str, title: &str, status: ToolCallStatus) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, title).status(status),
    ))
}

fn tool_done_frame(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
    )))
}

/// A ToolProgress tick lands in bash_progress only while the call is in the
/// running set; the chip reads this to render (Ns) / (Ns · M lines). Pins
/// the dispatch side of the bash-progress channel.
#[test]
fn test_tool_records_while_running() {
    let mut app = composition::app();
    // Not yet running: a stray tick (out of order) is dropped, not stored.
    app.handle_agent_message(AgentMessage::ToolProgress {
        call_id: "call_1".into(),
        elapsed_secs: 3,
        lines: None,
    });
    assert!(
        app.bash_progress.is_empty(),
        "tick before running is dropped"
    );
    // Running: the tick lands (elapsed + optional lines).
    app.handle_agent_message(AgentMessage::Frame(tool_call_frame(
        "call_1",
        "bash",
        ToolCallStatus::InProgress,
    )));
    app.handle_agent_message(AgentMessage::ToolProgress {
        call_id: "call_1".into(),
        elapsed_secs: 5,
        lines: Some(14),
    });
    assert_eq!(
        app.bash_progress.get("call_1"),
        Some(&BashProgress {
            elapsed_secs: 5,
            lines: Some(14)
        })
    );
    // Result lands: retire_tool clears the entry.
    app.handle_agent_message(AgentMessage::Frame(tool_done_frame("call_1")));
    assert!(!app.bash_progress.contains_key("call_1"));
}
