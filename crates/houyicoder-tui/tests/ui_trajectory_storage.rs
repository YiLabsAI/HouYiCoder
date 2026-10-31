//! Real-binary PTY tests for the /trajectory storage account: a resumed
//! session whose durable log holds a capture, an assembly, and a read, driven
//! through the real key routing at a wide and a narrow terminal width.

#![allow(clippy::unwrap_in_result)]

use crate::common::{self, Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir};
use houyicoder_context::{
    CachedModelView, CachedRead, ReadActor, ReadOutcome, SessionEvent, ViewForm,
};
use serde_json::json;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

const BLOCK: &str = "a1b2c3d4e5f60718";

/// A session whose one turn ran a shell command whose output was kept in the
/// block store: the model view carried a marker for it, the request assembled
/// the marker as a preview, and a later read opened one stream of it.
fn seed_stored_session(root: &Path, sid: &str) {
    let envelope = json!({
        "stdout": {
            "block_ref": BLOCK,
            "bytes": 23_040u64,
            "preview": "running 2432 tests"
        },
        "stderr": "",
        "exit_code": 0,
        "raw_capture": {
            "record": { "block_ref": "0f0f0f0f0f0f0f0f" },
            "stdout_bytes": 100_000u64,
            "stderr_bytes": 2_400u64,
            "stdout": { "block_ref": "1111111111111111" },
            "stderr": { "block_ref": "2222222222222222" }
        }
    });
    let events = [
        SessionEvent::UserInput {
            text: "run the suite".into(),
        },
        SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "bash".into(),
            input: json!({ "command": "cargo test" }),
        },
        SessionEvent::tool_result("c1", envelope),
        SessionEvent::CasModelView {
            assembly_id: 7,
            views: vec![CachedModelView {
                block_ref: BLOCK.into(),
                form: ViewForm::Summarize,
                size: 23_040,
            }],
            estimate_unit: "bytes".into(),
        },
        SessionEvent::CasRead {
            reads: vec![CachedRead {
                result_id: "c1:stdout".into(),
                actor: ReadActor::Model,
                start: 0,
                bytes: 4_096,
                outcome: ReadOutcome::Read,
            }],
        },
    ];
    common::seed_session_events(root, sid, "traj-model", &events);
}

/// Resume the seeded session and open its trajectory pane on the turn list.
fn resume_pane(root: &Path, sid: &str) -> PtySession {
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        root.to_path_buf(),
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after sid resume:\n{}",
        s.output()
    );
    s.clear_output();
    common::run_slash_command(&mut s, "trajectory");
    assert!(
        s.wait_for_plain("1 turn ", RENDER_TIMEOUT),
        "the seeded turn is listed:\n{}",
        s.output_plain()
    );
    s
}

/// A short pause so the pane has drawn the row the next key acts on.
fn sleep_for_render() {
    sleep(Duration::from_millis(300));
}

/// The timeline marks the call whose output went to the store, and the detail
/// says which block holds it, how much was kept raw, and what the request
/// carried in its place.
#[test]
#[ignore]
fn test_trajectory_shows_stored_block() {
    let root = fresh_temp_dir("sessions-traj-cas");
    let sid = "1b1b1b1b-1b1b-1b1b-1b1b-1b1b1b1b1b1b";
    seed_stored_session(&root, sid);
    let mut s = resume_pane(&root, sid);
    // Enter opens the turn's timeline; the shell result is its second record,
    // after the prompt that opened the turn.
    s.send_key(&Key::Enter);
    // The records load from disk after the drill opens; wait for the storage
    // row's kind label before moving the cursor, or the Down lands before the
    // list exists and the next Enter opens the wrong record.
    assert!(
        s.wait_for_plain("storage", RENDER_TIMEOUT),
        "the timeline loaded its records:\n{}",
        s.output_plain()
    );
    s.send_key(&Key::Down);
    sleep_for_render();
    assert!(
        s.wait_for_plain("[CAS 22.5 KiB]", RENDER_TIMEOUT),
        "the timeline row marks the kept block:\n{}",
        s.output_plain()
    );
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_compact("CASblocka1b2c3d4", RENDER_TIMEOUT),
        "the detail names the block:\n{}",
        s.output_plain()
    );
    assert!(
        s.wait_for_compact("100.0KiBraw", RENDER_TIMEOUT),
        "the detail states what the capture kept:\n{}",
        s.output_plain()
    );
    assert!(
        s.wait_for_compact("tier:preview", RENDER_TIMEOUT),
        "the detail states the form the request carried:\n{}",
        s.output_plain()
    );
    drop(s);
}

/// The same account survives a narrow terminal: the row drops its time axis
/// and the tag yields where there is no room for it, while the detail keeps
/// the block, the raw size, and the form, wrapped to the pane.
#[test]
#[ignore]
fn test_narrow_pane_shows_storage() {
    let root = fresh_temp_dir("sessions-traj-cas-narrow");
    let sid = "1c1c1c1c-1c1c-1c1c-1c1c-1c1c1c1c1c1c";
    seed_stored_session(&root, sid);
    let mut s = resume_pane(&root, sid);
    s.resize(24, 40);
    sleep_for_render();
    s.send_key(&Key::Enter);
    // The records load from disk after the drill opens; wait for the storage
    // row's kind label before moving the cursor, or the Down lands before the
    // list exists and the next Enter opens the wrong record.
    assert!(
        s.wait_for_plain("storage", RENDER_TIMEOUT),
        "the timeline loaded its records:\n{}",
        s.output_plain()
    );
    s.send_key(&Key::Down);
    sleep_for_render();
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("storage:", RENDER_TIMEOUT),
        "the narrow detail keeps the storage account:\n{}",
        s.output_plain()
    );
    // Back on the timeline, the read the model made still has its own row.
    s.send_key(&Key::Esc);
    sleep_for_render();
    s.send_key(&Key::Down);
    assert!(
        s.wait_for_plain("storage model", RENDER_TIMEOUT),
        "the narrow timeline still lists the read:\n{}",
        s.output_plain()
    );
    drop(s);
}
