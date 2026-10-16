//! Real-binary PTY tests for the /context command surface. The unit layer
//! (view/context_view/tests.rs) asserts on App
//! state + wire struct fields; this layer drives the real houyi binary through
//! a real terminal so the rendered byte stream the user actually sees is
//! fixed: every section is present, the legend carries the model/window/pct
//! line + category rows, a session that has run no turn still reports the
//! categories it would send (the "no data yet" empty-grid guard does not fire
//! on a real session because app.session is always Some), a resumed session
//! reports the history it loaded, and a post-compact session shows the folded
//! summary + Compact buffer category + Cache prefix line.
//!
//! Run via make suite ui (builds the bin first) or
//! cargo test --test ui_all ui_context:: -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

use crate::common::{
    self, Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, pty_session, pty_session_in_dir,
    pty_session_with_home, run_slash_command,
};
use std::fs;

/// /context renders every section on a fresh zero-turn session. The
/// composition root always attaches a live session, so /context reads the
/// breakdown that session measures rather than the stub the no-session path
/// would render. That stub path carries invented numbers, is unreachable in
/// production, and is not asserted here. The PTY session picks local mode
/// (sends '3' on the login screen) to land on the working screen without a
/// network call.
#[test]
#[ignore]
fn test_context_renders_sections() {
    let mut s = pty_session();
    s.clear_output();
    run_slash_command(&mut s, "context");
    assert!(
        s.wait_for("Context Usage", RENDER_TIMEOUT),
        "bold header missing:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("Estimated usage by category"),
        "legend header missing:\n{}",
        s.output()
    );
    // A real category row (Free space renders when free tokens > 0, which
    // holds for a fresh session with a large window + tiny system prompt).
    // Asserting a real label distinguishes the category row from the
    // model/window/pct legend line, which "tokens (" alone cannot.
    assert!(
        s.output().contains("Free space:"),
        "free-space category row missing:\n{}",
        s.output()
    );
    // Cache prefix renders unconditionally (dispatch always fills it from
    // System prompt + Tools tokens). At zero turns hit_rate is None (no
    // prior provider turn) — this asserts the decoupled prefix line still
    // shows, the exact case the wire-render bug hid. Pins the fix in the
    // happy path, not just the post-compact path.
    assert!(
        s.output().contains("Cache prefix:"),
        "cache prefix line missing on a zero-turn session:\n{}",
        s.output()
    );
    // The empty-grid guard ("no data yet") must NOT fire on a real wired
    // session even with zero turns — the measured breakdown is non-empty.
    assert!(
        !s.output().contains("no data yet"),
        "empty-grid guard fired on a real session:\n{}",
        s.output()
    );
}

/// /context after a compaction shows the folded summary line + the
/// Compact buffer category + the Cache prefix line. Seeds a session with a
/// checkpoint manifest on disk (a post-compact state) via the real
/// LocalFileBackend, launches --resume <sid>, and pins that the /context
/// render surfaces the post-compact state — the interaction the unit layer
/// could not reach because it never drove compact-then-/context through the
/// real dispatch + render path.
#[test]
#[ignore]
fn test_post_compact_shows_folded() {
    let sessions_dir = fresh_temp_dir("sessions-ctx-compact");
    let sid = "44444444-4444-4444-4444-444444444444";
    common::seed_session_with_checkpoint(
        &sessions_dir,
        sid,
        "ctx-compact-model",
        "seeded prompt",
        "summary of folded turns",
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir,
    );
    if !s.wait_for("let's build, or / for commands", RENDER_TIMEOUT) {
        panic!("working screen after resume:\n{}", s.output());
    }
    s.clear_output();
    run_slash_command(&mut s, "context");
    assert!(
        s.wait_for("Context Usage", RENDER_TIMEOUT),
        "bold header missing:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("Compact buffer"),
        "compact buffer category missing:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("folded:"),
        "folded summary line missing:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("Cache prefix:"),
        "cache prefix line missing:\n{}",
        s.output()
    );
    // The cache breakpoint bar (U+2502) renders on the grid at the cell where
    // the cached prefix ends — the real-binary verification of the breakpoint
    // marker.
    assert!(
        s.output().contains('\u{2502}'),
        "cache breakpoint bar missing on the grid:\n{}",
        s.output()
    );
}

/// /context on a resumed session reports the history the resume loaded, before
/// any new message is sent. The measurement reads the session as the store
/// holds it, so reopening a conversation does not collapse the pane to the
/// prompt and its tools.
#[test]
#[ignore]
fn test_resume_reports_loaded_history() {
    let sessions_dir = fresh_temp_dir("sessions-ctx-resume");
    let sid = "55555555-5555-5555-5555-555555555555";
    common::seed_session_with_usage(
        &sessions_dir,
        sid,
        "ctx-resume-model",
        "how does the cache work",
        common::SeededTurn {
            input: 12_000,
            output: 300,
            cache_read: 9_000,
        },
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir,
    );
    if !s.wait_for("let's build, or / for commands", RENDER_TIMEOUT) {
        panic!("working screen after resume:\n{}", s.output());
    }
    s.clear_output();
    run_slash_command(&mut s, "context");
    assert!(
        s.wait_for("Context Usage", RENDER_TIMEOUT),
        "bold header missing:\n{}",
        s.output()
    );
    // The loaded turn is the discriminator: a pane measured from an empty
    // event list reports the prompt and its tools only, and no messages row.
    assert!(
        s.output().contains("Messages:"),
        "the loaded history has no row:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("System prompt:") && s.output().contains("System tools:"),
        "the prompt and its tools have no rows:\n{}",
        s.output()
    );
}

/// The drill-down rows render in the real binary. A project holding a memory
/// file and a project skill must show both under /context: the measurement
/// reads them, ships them over the protocol, and the renderer draws what it
/// was told. The unit layer proves the renderer and the converter from
/// constructed data; only the real binary proves the dispatch filled the rows
/// instead of leaving them empty.
#[test]
#[ignore]
fn test_context_renders_drilldown_rows() {
    let dir = fresh_temp_dir("ctx-drill");
    // The composition root resolves the workspace by walking up from the
    // process cwd to the nearest Cargo.toml, so without one here the workspace
    // (and with it the skill scan) would land outside this temp dir.
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"ctx-drill\"\n")
        .expect("seed the manifest");
    fs::write(
        dir.join("AGENTS.md"),
        "# Project rules\nKeep the release notes short.\n",
    )
    .expect("seed the project memory file");
    let skill_dir = dir.join(".houyicoder/skills/release-check");
    fs::create_dir_all(&skill_dir).expect("seed the project skill dir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: release-check\ndescription: Check the release checklist before tagging.\n---\n\nbody\n",
    )
    .expect("seed the project skill");

    let mut s = pty_session_in_dir(dir);
    s.clear_output();
    run_slash_command(&mut s, "context");
    assert!(
        s.wait_for("Context Usage", RENDER_TIMEOUT),
        "bold header missing:\n{}",
        s.output()
    );
    // The seeded names are the discriminator: they exist only in this
    // project, so their presence means the drill-down was filled from the
    // measurement rather than left empty or taken from a pane cache. The
    // suffix makes the assertion the drill header, not the legend row that
    // carries the same label.
    assert!(
        s.wait_for("\u{00b7} /memory", RENDER_TIMEOUT),
        "memory drill-down section missing:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("AGENTS.md", RENDER_TIMEOUT),
        "the project memory file row missing:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("release-check", RENDER_TIMEOUT),
        "the project skill row missing:\n{}",
        s.output()
    );
}

/// /context reports what the session's own tools cost it and what its children
/// returned. Seeds a log holding a large shell result, a large file read, and a
/// child that reported its totals, then asserts the rendered advice and the
/// delegated rows. Only the real binary proves the dispatch measured the
/// footprints and folded the child's totals instead of shipping an empty
/// section: the unit layer renders constructed rows.
#[test]
#[ignore]
fn test_context_reports_measured_cost() {
    let sessions_dir = fresh_temp_dir("sessions-ctx-cost");
    let sid = "66666666-6666-6666-6666-666666666666";
    // Sized against the fallback window a test model resolves to: the shell
    // result takes more than the tool band, the read stays under it and over
    // the read band.
    let bash_result = "findings from the shell command\n".repeat(6_000);
    let read_result = "a line of the file the model read\n".repeat(3_000);
    common::seed_session_with_context_cost(
        &sessions_dir,
        sid,
        "ctx-cost-model",
        "measure what this session costs",
        &common::SeededContextCost {
            bash_result: &bash_result,
            read_result: &read_result,
            child_summary: "the child found three call sites",
            child_input: 900,
            child_output: 100,
        },
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir,
    );
    if !s.wait_for("let's build, or / for commands", RENDER_TIMEOUT) {
        panic!("working screen after resume:\n{}", s.output());
    }
    s.clear_output();
    run_slash_command(&mut s, "context");
    // The block is taller than the viewport once the drill-down and suggestion
    // rows are in it, so its header is painted above the visible region and
    // cannot be an anchor here; the first drill section stands in for "the
    // block rendered".
    assert!(
        s.wait_for("Delegated", RENDER_TIMEOUT),
        "the delegated section missing:\n{}",
        s.output()
    );
    for marker in [
        "Bash results using",
        "File reads using",
        "in context:",
        "returns: 1",
        "children used:",
    ] {
        assert!(
            s.wait_for(marker, RENDER_TIMEOUT),
            "the measured cost is missing {marker}:\n{}",
            s.output()
        );
    }
    // The child reported 900 input + 100 output, so the row reports that total
    // rather than reading unknown.
    assert!(
        !s.output().contains("children used: unknown"),
        "a child that reported its totals is not unknown:\n{}",
        s.output()
    );
}

/// The /context view across a session's life: before any turn, after a turn,
/// and after a clear.
#[test]
#[ignore]
fn test_context_lifecycle_rows() {
    let dir = fresh_temp_dir("ctx-life");
    let home = fresh_temp_dir("ctx-life-home");
    // The composition root walks up from the cwd to the nearest Cargo.toml for
    // the workspace, so the project fixtures need one to be seen at all.
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"ctx-life\"\n")
        .expect("seed the manifest");
    fs::write(
        dir.join("AGENTS.md"),
        "# Project rules\nKeep the release notes short.\n",
    )
    .expect("seed the project memory file");
    let skill_dir = dir.join(".houyicoder/skills/release-check");
    fs::create_dir_all(&skill_dir).expect("seed the project skill dir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: release-check\ndescription: Check the release checklist before tagging.\n---\n\nbody\n",
    )
    .expect("seed the project skill");
    // Stored memories in the project scope: what /memory lists, and what the
    // system prompt index carries one line each for.
    let memory_dir = dir.join(".houyicoder/memory");
    fs::create_dir_all(&memory_dir).expect("seed the memory root");
    for key in ["onboarding-notes", "build-recipes", "style-guide"] {
        fs::write(
            memory_dir.join(format!("{key}.md")),
            format!(
                "---\nname: {key}\ndescription: How this project is set up\nsource: Project\norigin: Manual\n---\nHow this project is set up.\n"
            ),
        )
        .expect("seed the stored memory");
    }

    let mut s = pty_session_with_home(dir, home, common::ONE_REPLY_SCRIPT);

    // Before any turn has run: the measurement reads the session as the store
    // holds it, so it reports the declared tool schemas rather than a zero the
    // first turn then contradicts, and the drill-down carries the project file,
    // the frozen index, and the project skill.
    run_slash_command(&mut s, "context");
    for marker in [
        "System prompt:",
        "System tools:",
        // The legend rows, not the drill headers below them: the category a
        // user reads off the grid carries the memory and the skills on a
        // session that has run no turn yet.
        "Memory files:",
        "Skills:",
        "AGENTS.md",
        "Memory index (3 entries)",
        "release-check",
    ] {
        assert!(
            s.wait_for_screen(marker, RENDER_TIMEOUT),
            "first open is missing {marker}:\n{}",
            s.screen().contents()
        );
    }
    // The drill-down is file-level: one row stands for the whole frozen index
    // however many memories it holds, so the per-key expansion that grew the
    // pane with the memory store cannot come back.
    for key in ["onboarding-notes", "build-recipes", "style-guide"] {
        assert!(
            !s.screen().contents().contains(key),
            "the drill-down expands no individual memory key, found {key}:\n{}",
            s.screen().contents()
        );
    }
    assert!(
        !s.screen().contents().contains("Messages:"),
        "no turn has run, so there is no messages row:\n{}",
        s.screen().contents()
    );

    // After a turn: the status bar carries the occupancy, and the breakdown
    // reports the conversation the message row measures.
    s.send_str("hello");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("% context used", RENDER_TIMEOUT),
        "the status bar reports the measured occupancy:\n{}",
        s.screen().contents()
    );
    run_slash_command(&mut s, "context");
    assert!(
        s.wait_for_screen("Messages:", RENDER_TIMEOUT),
        "a turn ran, so the messages row reports its size:\n{}",
        s.screen().contents()
    );

    // After the session is archived: the fresh session measures its own empty
    // history, so the archived session's message row is gone.
    run_slash_command(&mut s, "clear");
    assert!(
        s.wait_for_screen("session archived, new session started", RENDER_TIMEOUT),
        "clear archives the session:\n{}",
        s.screen().contents()
    );
    s.clear_output();
    run_slash_command(&mut s, "context");
    assert!(
        s.wait_for("System tools:", RENDER_TIMEOUT),
        "the fresh session still measures the tool schemas:\n{}",
        s.output()
    );
    assert!(
        !s.output().contains("Messages:"),
        "the archived session's messages row must not survive the clear:\n{}",
        s.output()
    );
}
