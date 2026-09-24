//! Demonstration trajectory data for the timeline pane.
//!
//! Provides a static demonstration view when running without a live
//! session log reader attached.

use super::*;

pub(crate) fn sample_trajectory() -> TrajectoryView {
    // A realistic houyi work session — fixing the permission pipeline crash,
    // then wiring the trajectory pane, then PTY-testing the journey. Real
    // paths, real commands, real-ish timings + start offsets. start_ms
    // positions each event on the shared time axis (mostly sequential, as the
    // agent runs tools per-completion; the axis still shows where time went).
    let t1 = TrajectoryTurn {
        key: TrajectoryTurnKey::from_opening_event("sample-1"),
        n: 1,
        boundary_before: Vec::new(),
        user_input: "fix the permission pipeline crash".into(),
        title: "fix the permission pipeline crash".into(),
        tokens_in: Some(3200),
        tokens_out: Some(800),
        cache_read: Some(2400),
        cache_write: Some(0),
        models: Vec::new(),
        efforts: Vec::new(),
        reasoning_tokens: None,
        tool_count: 4,
        tool_fail: 1,
        retries: 0,
        duration_ms: 12400,
        success: true,
    };
    let t2 = TrajectoryTurn {
        key: TrajectoryTurnKey::from_opening_event("sample-2"),
        n: 2,
        boundary_before: Vec::new(),
        user_input: "wire the trajectory pane 3-level drill".into(),
        title: "wire the trajectory pane 3-level drill".into(),
        tokens_in: Some(5100),
        tokens_out: Some(1200),
        cache_read: Some(0),
        cache_write: Some(0),
        models: Vec::new(),
        efforts: Vec::new(),
        reasoning_tokens: None,
        tool_count: 5,
        tool_fail: 0,
        retries: 0,
        duration_ms: 8400,
        success: true,
    };
    let t3 = TrajectoryTurn {
        key: TrajectoryTurnKey::from_opening_event("sample-3"),
        n: 3,
        boundary_before: Vec::new(),
        user_input: "pty test the drill journey".into(),
        title: "pty test the drill journey".into(),
        tokens_in: Some(2800),
        tokens_out: Some(2100),
        cache_read: Some(0),
        cache_write: Some(0),
        models: Vec::new(),
        efforts: Vec::new(),
        reasoning_tokens: None,
        tool_count: 2,
        tool_fail: 0,
        retries: 0,
        duration_ms: 5800,
        success: true,
    };
    TrajectoryView {
        state: super::TrajectoryViewState::Ready,
        skipped_records: 0,
        models_used: 2,
        tool_calls: 4,
        session_id: "a1b2c3".into(),
        model: "qwen3.7-max".into(),
        total_turns: 3,
        tokens_in: Some(11100),
        tokens_out: Some(4100),
        failures: 1,
        duration_secs: 39,
        cache_read: Some(9800),
        timing: SessionTiming {
            ttft_samples: 12,
            ttft_avg_ms: Some(420),
            ttft_p95_ms: Some(650),
            ttft_p99_ms: Some(780),
            decode_samples: 12,
            decode_tok_per_sec: Some(38.5),
            model_ms: 12400,
            tool_ms: 3200,
        },
        hidden_turns: 0,
        newer_hidden: 0,
        history_generation: 0,
        subagent_usage: None,
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
    kind: TrajectoryRecordKind,
    name: Option<&str>,
    summary: &str,
    start_ms: u64,
    ms: u64,
    ok: bool,
    thinking: Option<&str>,
    input: Option<&str>,
    output: Option<&str>,
) -> TrajectoryRecord {
    TrajectoryRecord {
        kind,
        name: name.map(Into::into),
        ordinal: 0,
        summary: summary.into(),
        start_ms,
        duration_ms: ms,
        outcome: if ok {
            RecordOutcome::Ok
        } else {
            RecordOutcome::Failed
        },
        thinking: thinking.map(Into::into),
        input: input.map(Into::into),
        output: output.map(Into::into),
        usage: None,
        timing: None,
        retries: 0,
    }
}
fn bg(kind: &str, summary: &str, ms: u64) -> TrajectoryBg {
    TrajectoryBg {
        kind: kind.into(),
        summary: summary.into(),
        duration_ms: ms,
    }
}

/// The records of demonstration turn 1.
#[expect(clippy::too_many_lines, reason = "a realistic turn, kept whole")]
fn sample_records_1() -> Vec<TrajectoryRecord> {
    vec![
        ev(
            TrajectoryRecordKind::Model,
            Some("qwen3.7-max"),
            "thinking (3.2k↓ 0.8k↑ cache 2.4k)",
            0,
            2100,
            true,
            Some(
                "The crash is in the permission pipeline. The user says session-scope\n\
                     consent does not persist across resume. Looking at the gate: the\n\
                     ConsentStore is keyed by exact tool input, but git commit messages\n\
                     differ each time, so the consent never matches. A session-scope\n\
                     allow-rule (memory-only, not persisted) with prefix content\n\
                     matching would solve this. I should add a\n\
                     Scope::Session variant — memory-only, not persisted — and seed a\n\
                     session allow-rule on consent. Need to check store.rs and gate.rs.",
            ),
            None,
            None,
        ),
        ev(
            TrajectoryRecordKind::Memory,
            None,
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
            TrajectoryRecordKind::Tool,
            Some("read"),
            "crates/houyicoder-permission/src/gate.rs",
            2400,
            100,
            true,
            None,
            Some("crates/houyicoder-permission/src/gate.rs:1-180"),
            None,
        ),
        ev(
            TrajectoryRecordKind::Tool,
            Some("edit"),
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
            TrajectoryRecordKind::Tool,
            Some("bash"),
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
            TrajectoryRecordKind::Error,
            Some("hook"),
            "deny: no-backticks (#2)",
            5980,
            0,
            false,
            None,
            None,
            Some("rule: no-backticks · count: 2"),
        ),
        ev(
            TrajectoryRecordKind::Tool,
            Some("edit"),
            "crates/houyicoder-permission/src/gate.rs (retry)",
            5980,
            90,
            true,
            None,
            Some("- _ => Scope::User,\n+ Scope::Session => ... seeded on consent"),
            None,
        ),
        ev(
            TrajectoryRecordKind::Tool,
            Some("bash"),
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
            TrajectoryRecordKind::Model,
            Some("qwen3.7-max"),
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
    ]
}

/// The records of demonstration turn 2.
#[expect(clippy::too_many_lines, reason = "a realistic turn, kept whole")]
fn sample_records_2() -> Vec<TrajectoryRecord> {
    vec![
        ev(
            TrajectoryRecordKind::Model,
            Some("qwen3.7-max"),
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
            TrajectoryRecordKind::Memory,
            None,
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
            TrajectoryRecordKind::Tool,
            Some("read"),
            "wiki/design/feature/observability-design.md",
            3400,
            50,
            true,
            None,
            Some("wiki/design/feature/observability-design.md §5.5"),
            None,
        ),
        ev(
            TrajectoryRecordKind::Tool,
            Some("edit"),
            "crates/houyicoder-tui/src/view/trajectory_pane.rs",
            3450,
            150,
            true,
            None,
            Some("+ fn draw_turn_detail + positioned_bar + render_scrolled"),
            None,
        ),
        ev(
            TrajectoryRecordKind::Tool,
            Some("edit"),
            "crates/houyicoder-tui/src/state.rs",
            3600,
            120,
            true,
            None,
            Some("+ trajectory: TrajectoryPaneState,"),
            None,
        ),
        ev(
            TrajectoryRecordKind::Tool,
            Some("bash"),
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
            TrajectoryRecordKind::Model,
            Some("qwen3.7-max"),
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
    ]
}

/// The records of demonstration turn 3.
fn sample_records_3() -> Vec<TrajectoryRecord> {
    vec![
        ev(
            TrajectoryRecordKind::Model,
            Some("qwen3.7-max"),
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
            TrajectoryRecordKind::Tool,
            Some("edit"),
            "crates/houyicoder-tui/tests/ui_fence.rs",
            1500,
            200,
            true,
            None,
            Some("+ fn trajectory_drills_three_levels + trajectory_visual_capture"),
            None,
        ),
        ev(
            TrajectoryRecordKind::Tool,
            Some("bash"),
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
            TrajectoryRecordKind::Model,
            Some("qwen3.7-max"),
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
    ]
}

/// The records of a demonstration turn, or a detail that says the key is not
/// one of them.
pub(crate) fn sample_detail(key: &TrajectoryTurnKey) -> TrajectoryDetailView {
    for (n, records) in [
        ("sample-1", sample_records_1()),
        ("sample-2", sample_records_2()),
        ("sample-3", sample_records_3()),
    ] {
        if key.as_str() == n {
            return TrajectoryDetailView {
                state: TrajectoryDetailState::Ready,
                records,
                truncated: false,
            };
        }
    }
    TrajectoryDetailView {
        state: TrajectoryDetailState::Stale,
        records: Vec::new(),
        truncated: false,
    }
}
