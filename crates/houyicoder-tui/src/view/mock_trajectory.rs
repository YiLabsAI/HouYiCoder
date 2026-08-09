//! Mock trajectory data for the /trajectory pane, extracted to keep the
//! render module under the file-size gate. Included via a path attribute so
//! it sees the parent module types via super::*.

use super::*;

// Mock data

#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
pub(crate) fn mock_trajectory() -> TrajectoryView {
    // A realistic houyi work session — fixing the permission pipeline crash,
    // then wiring the trajectory pane, then PTY-testing the journey. Real
    // paths, real commands, real-ish timings + start offsets. start_ms
    // positions each event on the shared time axis (mostly sequential, as the
    // agent runs tools per-completion; the axis still shows where time went).
    let t1 = TrajectoryTurn {
        n: 1,
        user_input: "fix the permission pipeline crash".into(),
        tokens_in: Some(3200),
        tokens_out: Some(800),
        cache_read: Some(2400),
        cache_write: Some(0),
        model: None,
        effort: None,
        reasoning_tokens: None,
        tool_count: 4,
        tool_fail: 1,
        retries: 0,
        duration_ms: 12400,
        success: true,
        events: vec![
            ev(
                "llm",
                "thinking (3.2k↓ 0.8k↑ cache 2.4k)",
                0,
                2100,
                true,
                Some(
                    "The crash is in the permission pipeline. The user says session-scope\n\
                     consent does not persist across resume. Looking at the gate: the\n\
                     ConsentStore is keyed by exact tool input, but git commit messages\n\
                     differ each time, so the consent never matches. CC solves this with\n\
                     an in-memory session rule with prefix content. I should add a\n\
                     Scope::Session variant — memory-only, not persisted — and seed a\n\
                     session allow-rule on consent. Need to check store.rs and gate.rs.",
                ),
                None,
                None,
            ),
            ev(
                "recall",
                "3 keys (permission-pipeline, consent-store, gate-decide) 12.3KB",
                2100,
                300,
                true,
                None,
                None,
                Some(
                    "permission-pipeline: ConsentStore exact-param, no prefix match\n\
                     consent-store: keys by (tool, input hash)\n\
                     gate-decide: Scope enum lacks Session variant",
                ),
            ),
            ev(
                "read",
                "crates/houyicoder-permission/src/gate.rs",
                2400,
                100,
                true,
                None,
                Some("crates/houyicoder-permission/src/gate.rs:1-180"),
                None,
            ),
            ev(
                "edit",
                "crates/houyicoder-permission/src/gate.rs",
                2500,
                80,
                true,
                None,
                Some(
                    "- pub enum Scope { User, Project, Local }\n+ pub enum Scope { User, Project, Local, Session }",
                ),
                None,
            ),
            ev(
                "bash",
                "cargo test -p houyicoder-permission",
                2580,
                3400,
                false,
                None,
                Some("cargo test -p houyicoder-permission"),
                Some(
                    "running 3 tests\n\
                     test gate::decide_git_checkpoint ... FAILED\n\
                     test gate::consent_store ... FAILED\n\
                     test gate::scope_session ... FAILED\n\
                     \n\
                     failures:\n\
                     ---- decide_git_checkpoint stdout ----\n\
                     panic: scope Session not found in match\n\
                     \n\
                     3 failed; 0 passed; finished in 3.3s\n\
                     exit code 1",
                ),
            ),
            ev(
                "gate",
                "deny: no-backticks (#2)",
                5980,
                0,
                false,
                None,
                None,
                Some("rule: no-backticks · count: 2"),
            ),
            ev(
                "edit",
                "crates/houyicoder-permission/src/gate.rs (retry)",
                5980,
                90,
                true,
                None,
                Some("- _ => Scope::User,\n+ Scope::Session => ... seeded on consent"),
                None,
            ),
            ev(
                "bash",
                "cargo test -p houyicoder-permission",
                6070,
                3100,
                true,
                None,
                Some("cargo test -p houyicoder-permission"),
                Some(
                    "running 3 tests\n\
                     test gate::decide_git_checkpoint ... ok\n\
                     test gate::consent_store ... ok\n\
                     test gate::scope_session ... ok\n\
                     \n\
                     3 passed; 0 failed; finished in 3.0s\n\
                     exit code 0",
                ),
            ),
            ev(
                "llm",
                "fixed the crash - session-scope consent now persists",
                9170,
                1200,
                true,
                Some(
                    "Root cause was ConsentStore keyed by exact input — git commit\n\
                     messages differ each commit so consent never matched. Added\n\
                     Scope::Session (memory-only, seeded on consent, matching the\n\
                     in-memory session rule). Tests green. The fix persists\n\
                     consent for the session without writing to disk, so resume stays\n\
                     clean.",
                ),
                None,
                Some(
                    "Fixed the permission pipeline crash. Session-scope consent now persists across the session (memory-only, seeded on consent). 3 tests pass.",
                ),
            ),
        ],
    };
    let t2 = TrajectoryTurn {
        n: 2,
        user_input: "wire the trajectory pane 3-level drill".into(),
        tokens_in: Some(5100),
        tokens_out: Some(1200),
        cache_read: Some(0),
        cache_write: Some(0),
        model: None,
        effort: None,
        reasoning_tokens: None,
        tool_count: 5,
        tool_fail: 0,
        retries: 0,
        duration_ms: 8400,
        success: true,
        events: vec![
            ev(
                "llm",
                "thinking (5.1k↓ 1.2k↑)",
                0,
                3200,
                true,
                Some(
                    "Need a 3-level drill: L0 turn list, L1 turn detail with a time\n\
                     axis, L2 event detail. This pane is the drill-down surface.\n\
                     For the time axis I will use a\n\
                     positional Gantt — bars positioned at start offset on a shared\n\
                     axis, not ASCII hash proportions. Unicode block elements. Header\n\
                     and footer pinned, body scrolls to follow cursor.",
                ),
                None,
                None,
            ),
            ev(
                "recall",
                "2 keys (trajectory-ux, observability-design) 8.1KB",
                3200,
                200,
                true,
                None,
                None,
                Some(
                    "trajectory-ux: 3-level drill, positional Gantt\nobservability-design: §5.5 trajectory pane spec",
                ),
            ),
            ev(
                "read",
                "docs/design/feature/observability-design.md",
                3400,
                50,
                true,
                None,
                Some("docs/design/feature/observability-design.md §5.5"),
                None,
            ),
            ev(
                "edit",
                "crates/houyicoder-tui/src/view/trajectory_pane.rs",
                3450,
                150,
                true,
                None,
                Some("+ fn draw_turn_detail + positioned_bar + render_scrolled"),
                None,
            ),
            ev(
                "edit",
                "crates/houyicoder-tui/src/state.rs",
                3600,
                120,
                true,
                None,
                Some("+ trajectory_level: Cell<u8>, trajectory_cursor: Cell<usize>"),
                None,
            ),
            ev(
                "bash",
                "cargo test -p houyicoder-tui --lib trajectory",
                3720,
                2800,
                true,
                None,
                Some("cargo test -p houyicoder-tui --lib trajectory"),
                Some(
                    "running 10 tests\n\
                     test trajectory_pane::level0_renders_turn_list ... ok\n\
                     test trajectory_pane::level1_renders_turn_detail ... ok\n\
                     test trajectory_pane::level2_renders_event_detail ... ok\n\
                     test trajectory_pane::enter_drill_esc_back ... ok\n\
                     test trajectory_pane::down_clamps_to_last_row ... ok\n\
                     10 passed; 0 failed; finished in 2.7s\n\
                     exit code 0",
                ),
            ),
            ev(
                "llm",
                "trajectory pane 3-level drill wired",
                6520,
                1800,
                true,
                Some(
                    "The 3-level drill + positional Gantt is wired and tests pass.\n\
                     Key design: render_scrolled pins header/footer and scrolls body\n\
                     to follow cursor, so the footer hints are never clipped no matter\n\
                     how many events a turn holds.",
                ),
                None,
                Some("Trajectory pane 3-level drill wired. 10 unit tests + PTY journey green."),
            ),
        ],
    };
    let t3 = TrajectoryTurn {
        n: 3,
        user_input: "pty test the drill journey".into(),
        tokens_in: Some(2800),
        tokens_out: Some(2100),
        cache_read: Some(0),
        cache_write: Some(0),
        model: None,
        effort: None,
        reasoning_tokens: None,
        tool_count: 2,
        tool_fail: 0,
        retries: 0,
        duration_ms: 5800,
        success: true,
        events: vec![
            ev(
                "llm",
                "thinking (2.8k↓ 2.1k↑)",
                0,
                1500,
                true,
                Some(
                    "Need a PTY test that drives the real binary through the full\n\
                     journey: open /trajectory, Enter to L1, Down to move cursor,\n\
                     Enter to L2, Esc back, Esc to close. The renderer is diff-based\n\
                     so footer tokens that share unchanged chars with the prior frame\n\
                     arrive split — use clear_output + level-unique tokens.",
                ),
                None,
                None,
            ),
            ev(
                "edit",
                "crates/houyicoder-tui/tests/ui_fence.rs",
                1500,
                200,
                true,
                None,
                Some("+ fn trajectory_drills_three_levels + trajectory_visual_capture"),
                None,
            ),
            ev(
                "bash",
                "cargo test --test ui_fence trajectory_drills -- --ignored",
                1700,
                3100,
                true,
                None,
                Some("cargo test --test ui_fence trajectory_drills -- --ignored"),
                Some(
                    "running 1 test\n\
                     test trajectory_drills_three_levels ... ok\n\
                     test result: ok. 1 passed; 0 failed; finished in 1.7s\n\
                     exit code 0",
                ),
            ),
            ev(
                "llm",
                "pty journey green",
                4800,
                1000,
                true,
                Some(
                    "PTY journey passes — the real binary drives the 3-level drill end to end through a real terminal.",
                ),
                None,
                Some("PTY journey green. Full 3-level drill verified through the real binary."),
            ),
        ],
    };
    TrajectoryView {
        session_id: "a1b2c3".into(),
        model: "qwen3.7-max".into(),
        total_turns: 3,
        tokens_in: Some(11100),
        tokens_out: Some(4100),
        failures: 1,
        duration_secs: 39,
        rows: vec![
            TrajectoryRow::Turn(t1),
            TrajectoryRow::Turn(t2),
            TrajectoryRow::Bg(bg("dream", "merged 3, deleted 1, promoted 1", 800)),
            TrajectoryRow::Bg(bg("compact", "8 folded (12.4KB to 2.1KB)", 300)),
            TrajectoryRow::Bg(bg("save", "2 memories saved (auto, extracted)", 100)),
            TrajectoryRow::Turn(t3),
        ],
    }
}

#[expect(clippy::too_many_arguments, reason = "param grouping deliberate")]
fn ev(
    kind: &str,
    summary: &str,
    start_ms: u64,
    ms: u64,
    ok: bool,
    thinking: Option<&str>,
    input: Option<&str>,
    output: Option<&str>,
) -> TrajectoryEvent {
    TrajectoryEvent {
        kind: kind.into(),
        summary: summary.into(),
        start_ms,
        duration_ms: ms,
        success: ok,
        thinking: thinking.map(Into::into),
        input: input.map(Into::into),
        output: output.map(Into::into),
    }
}
fn bg(kind: &str, summary: &str, ms: u64) -> TrajectoryBg {
    TrajectoryBg {
        kind: kind.into(),
        summary: summary.into(),
        duration_ms: ms,
    }
}
