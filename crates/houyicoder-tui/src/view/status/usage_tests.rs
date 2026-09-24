//! Tests for the Usage sub-tab: the token and cache rows, the latency rows
//! read from the session's typed timing summary, and the delegated-work row.

use super::render_usage;
use crate::command::render::format_tokens;
use crate::state::{TrajectoryDrill, TrajectoryTurnKey};
use crate::view::trajectory_pane::TrajectoryDetailView;
use crate::view::trajectory_pane::{SessionTiming, SubagentUsage, TrajectoryLog, TrajectoryView};

/// A trajectory source that answers a fixed view and does not page: the Usage
/// tab reads the session's figures from it and never walks.
struct Fixed(std::sync::Arc<TrajectoryView>);

impl TrajectoryLog for Fixed {
    fn trajectory(&self) -> std::sync::Arc<TrajectoryView> {
        self.0.clone()
    }
    fn load_older(&self) {}
    fn load_earliest(&self) {}
    fn return_to_tail(&self) {}
    fn request_detail(&self, _drill: &TrajectoryDrill) {}
    fn detail(&self, _key: &TrajectoryTurnKey) -> std::sync::Arc<TrajectoryDetailView> {
        std::sync::Arc::new(TrajectoryDetailView::default())
    }
}

/// The Usage tab renders the token counts from the snapshot.
#[test]
fn test_usage_tab_has_tokens() {
    let app = crate::test_harness::working_app();
    let s = render_usage(&app);
    assert!(s.contains("input tokens:"), "input row: {s}");
    assert!(s.contains("output tokens:"), "output row: {s}");
    assert!(s.contains("cached input:"), "cache row: {s}");
}

/// The Usage tab shows reasoning tokens (incl. in output) only when >0.
#[test]
fn test_usage_tab_shows_reasoning() {
    let mut app = crate::test_harness::working_app();
    let mut snap = app.snapshot_or_stub();
    snap.cumulative_usage.reasoning_tokens = 1500;
    app.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(s.contains("reasoning:"), "reasoning row: {s}");
    assert!(s.contains("incl. in output"), "inclusion note: {s}");

    // Hidden when 0.
    let mut app = crate::test_harness::working_app();
    let mut snap = app.snapshot_or_stub();
    snap.cumulative_usage.reasoning_tokens = 0;
    app.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(!s.contains("reasoning:"), "no reasoning when 0: {s}");
}

/// The Usage tab renders the per-model section only when two or more
/// models share the session; a single model is already covered by the
/// flat rows and a per-model section would just repeat them.
#[test]
fn test_single_omits_per_model() {
    let mut app = crate::test_harness::working_app();
    let mut snap = app.snapshot_or_stub();
    snap.by_model = vec![houyicoder_protocol::frontend::status::ModelUsageView {
        model: "glm-5.2".into(),
        input_tokens: 1000,
        output_tokens: 500,
        ..Default::default()
    }];
    app.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(
        !s.contains("Usage by model:"),
        "single model: no per-model section: {s}"
    );
}

/// The per-model section lists each model on its own line with the
/// token counts, sorted heaviest-first, reasoning only when that model
/// used any. Tokens render compact (k/m) so large counts fit one line.
#[test]
fn test_per_model_lists_models() {
    let mut app = crate::test_harness::working_app();
    let mut snap = app.snapshot_or_stub();
    snap.by_model = vec![
        houyicoder_protocol::frontend::status::ModelUsageView {
            model: "qwen3.7-max".into(),
            input_tokens: 1_500_000,
            output_tokens: 400_000,
            cache_read_tokens: 1_100_000,
            cache_write_tokens: 900_000,
            reasoning_tokens: 0,
        },
        houyicoder_protocol::frontend::status::ModelUsageView {
            model: "glm-5.2".into(),
            input_tokens: 300_000,
            output_tokens: 100_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 8_000,
        },
    ];
    app.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(s.contains("Usage by model:"), "section header: {s}");
    let lines: Vec<&str> = s.lines().collect();
    // Heaviest (qwen3.7-max, 1.9M in+out) leads glm-5.2 (400k).
    let qwen_idx = lines.iter().position(|l| l.contains("qwen3.7-max"));
    let glm_idx = lines.iter().position(|l| l.contains("glm-5.2"));
    assert!(qwen_idx < glm_idx, "heaviest model leads: {s}");
    let qwen = lines.iter().find(|l| l.contains("qwen3.7-max")).unwrap();
    let glm = lines.iter().find(|l| l.contains("glm-5.2")).unwrap();
    assert!(qwen.contains("1.5m input"), "compact m suffix: {qwen}");
    assert!(qwen.contains("400k output"), "compact k suffix: {qwen}");
    assert!(
        !qwen.contains("reasoning"),
        "qwen reasoning 0 omitted: {qwen}"
    );
    assert!(glm.contains("300k input"), "glm compact: {glm}");
    assert!(glm.contains("8k reasoning"), "glm reasoning shown: {glm}");
}

/// A model id wider than the label column still keeps a separating space,
/// so the row never runs the id and token count together.
#[test]
fn test_long_model_id_spacing() {
    let mut app = crate::test_harness::working_app();
    let mut snap = app.snapshot_or_stub();
    snap.by_model = vec![
        houyicoder_protocol::frontend::status::ModelUsageView {
            model: "deepseek-v4-pro-0813".into(),
            input_tokens: 2_000_000,
            ..Default::default()
        },
        houyicoder_protocol::frontend::status::ModelUsageView {
            model: "qwen3.7-max".into(),
            input_tokens: 1_000,
            ..Default::default()
        },
    ];
    app.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(
        s.contains("deepseek-v4-pro-0813: 2m input"),
        "long id keeps a space: {s}"
    );
    assert!(s.contains("cached"), "clear cache label: {s}");
    assert!(!s.contains("cache rd"), "no abbreviation: {s}");
}

/// Compact formatter: k and m suffixes with trailing .0 trimmed, raw
/// under 1000. Pins the render the Usage tab depends on.
#[test]
fn test_format_tokens_compact() {
    assert_eq!(format_tokens(0), "0");
    assert_eq!(format_tokens(999), "999");
    assert_eq!(format_tokens(1000), "1k");
    assert_eq!(format_tokens(16100), "16.1k");
    assert_eq!(format_tokens(1_600_000), "1.6m");
}

/// The Usage tab reports session latency from the same typed summary the
/// trajectory pane reads, and omits a row the session has no sample for.
#[test]
fn test_usage_tab_latency_rows() {
    let view = TrajectoryView {
        session_id: "s".into(),
        model: "m".into(),
        total_turns: 1,
        tokens_in: Some(10),
        tokens_out: Some(5),
        cache_read: None,
        failures: 0,
        duration_ms: Some(1_000),
        timing: SessionTiming {
            ttft_samples: 612,
            ttft_avg_ms: Some(1800),
            ttft_p95_ms: Some(4800),
            ttft_p99_ms: Some(8200),
            decode_samples: 590,
            decode_tok_per_sec: Some(31.4),
            model_ms: 91_200,
            tool_ms: 28_400,
        },
        hidden_turns: 0,
        subagent_usage: None,
        rows: Vec::new(),
        ..TrajectoryView::default()
    };
    let mut app = crate::test_harness::working_app();
    app.trajectory_log = Some(std::sync::Arc::new(Fixed(std::sync::Arc::new(view))));
    let s = render_usage(&app);
    assert!(s.contains("model / tool time:"), "work time row: {s}");
    assert!(s.contains("91.2s / 28.4s"), "the split: {s}");
    assert!(s.contains("ttft:"), "ttft row: {s}");
    assert!(
        s.contains("1.8s avg · 4.8s p95 · 8.2s p99"),
        "percentiles: {s}"
    );
    assert!(s.contains("(612 samples)"), "the sample count: {s}");
    assert!(s.contains("decode speed:"), "decode row: {s}");
    assert!(s.contains("31.4 tok/s (590 samples)"), "the rate: {s}");
    // The existing rows keep their names and order above the new ones.
    assert!(s.contains("input tokens:"), "existing rows intact: {s}");
    assert!(
        s.find("input tokens:").unwrap() < s.find("model / tool time:").unwrap(),
        "latency rows come after the token rows: {s}"
    );
}

/// The Usage tab prints a latency under a second in milliseconds, so the
/// row cannot read as a session that took no time while the trajectory
/// header reports the same sample as a span of milliseconds.
#[test]
fn test_usage_tab_short_latency() {
    let mut app = crate::test_harness::working_app();
    app.trajectory_log = Some(std::sync::Arc::new(Fixed(std::sync::Arc::new(
        TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 1,
            tokens_in: Some(10),
            tokens_out: Some(5),
            cache_read: None,
            failures: 0,
            duration_ms: Some(20),
            timing: SessionTiming {
                ttft_samples: 3,
                ttft_avg_ms: Some(20),
                ttft_p95_ms: Some(30),
                ttft_p99_ms: Some(40),
                decode_samples: 0,
                decode_tok_per_sec: None,
                model_ms: 20,
                tool_ms: 10,
            },
            hidden_turns: 0,
            subagent_usage: None,
            rows: Vec::new(),
            ..TrajectoryView::default()
        },
    ))));
    let s = render_usage(&app);
    assert!(s.contains("20ms / 10ms"), "work time in milliseconds: {s}");
    assert!(
        s.contains("20ms avg · 30ms p95 · 40ms p99"),
        "percentiles in milliseconds: {s}"
    );
    assert!(!s.contains("0.0s"), "no false zero: {s}");
}

/// With no timing recorded, the Usage tab shows no latency row at all: an
/// unmeasured session must not read as instant.
#[test]
fn test_usage_tab_no_timing() {
    let mut app = crate::test_harness::working_app();
    app.trajectory_log = Some(std::sync::Arc::new(Fixed(std::sync::Arc::new(
        TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 0,
            tokens_in: None,
            tokens_out: None,
            cache_read: None,
            failures: 0,
            duration_ms: Some(0),
            timing: SessionTiming::default(),
            hidden_turns: 0,
            subagent_usage: None,
            rows: Vec::new(),
            ..TrajectoryView::default()
        },
    ))));
    let s = render_usage(&app);
    assert!(!s.contains("ttft:"), "no ttft row without samples: {s}");
    assert!(!s.contains("decode speed:"), "no decode row: {s}");
    assert!(!s.contains("model / tool time:"), "no work-time row: {s}");
}

/// Delegated sub-agent work is reported on its own row, which says the
/// token rows above already include it.
#[test]
fn test_usage_tab_delegated() {
    let mut app = crate::test_harness::working_app();
    app.trajectory_log = Some(std::sync::Arc::new(Fixed(std::sync::Arc::new(
        TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 1,
            tokens_in: Some(10),
            tokens_out: Some(5),
            cache_read: None,
            failures: 0,
            duration_ms: Some(1_000),
            timing: SessionTiming::default(),
            hidden_turns: 0,
            subagent_usage: Some(SubagentUsage {
                calls: 2,
                input: 812_000,
                output: 41_000,
                cache_read: 755_000,
            }),
            rows: Vec::new(),
            ..TrajectoryView::default()
        },
    ))));
    let s = render_usage(&app);
    assert!(s.contains("delegated usage:"), "the row is present: {s}");
    assert!(s.contains("812k input"), "input shown: {s}");
    assert!(s.contains("41k output"), "output shown: {s}");
    assert!(s.contains("93% cached"), "the child cache share: {s}");
    assert!(
        s.contains("included above"),
        "the row says the totals above already include it: {s}"
    );
}

/// A session with no delegation shows no delegated row.
#[test]
fn test_usage_tab_no_delegated() {
    let mut app = crate::test_harness::working_app();
    app.trajectory_log = Some(std::sync::Arc::new(Fixed(std::sync::Arc::new(
        TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 1,
            tokens_in: Some(10),
            tokens_out: Some(5),
            cache_read: None,
            failures: 0,
            duration_ms: Some(1_000),
            timing: SessionTiming::default(),
            hidden_turns: 0,
            subagent_usage: None,
            rows: Vec::new(),
            ..TrajectoryView::default()
        },
    ))));
    let s = render_usage(&app);
    assert!(!s.contains("delegated usage:"), "no row: {s}");
}

/// The cached row divides only when input was reported. With input it
/// shows the share; without it the count stands alone rather than reading
/// as a cache miss.
#[test]
fn test_usage_cache_share() {
    use houyicoder_protocol::llm::Usage;
    let mut app = crate::test_harness::working_app();
    let mut snap = app.status_cache.take().unwrap_or_default();
    snap.model = "m".into();
    snap.cumulative_usage = Usage {
        input_tokens: 1000,
        total_tokens: 1000,
        cache_read_input_tokens: 500,
        ..Default::default()
    };
    app.status_cache = Some(snap.clone());
    let s = render_usage(&app);
    assert!(s.contains("50.0% of input"), "the share is shown: {s}");
    snap.cumulative_usage = Usage {
        cache_read_input_tokens: 500,
        ..Default::default()
    };
    app.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(s.contains("cached input"), "the row is present: {s}");
    assert!(s.contains("500"), "the count stands: {s}");
    assert!(!s.contains("% of input"), "no share to divide by: {s}");
}

/// A child that returned before reporting usage leaves the row saying so,
/// rather than printing zeroes for a cost that was never measured.
#[test]
fn test_usage_tab_delegated_unreported() {
    let mut app = crate::test_harness::working_app();
    app.trajectory_log = Some(std::sync::Arc::new(Fixed(std::sync::Arc::new(
        TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 1,
            tokens_in: Some(10),
            tokens_out: Some(5),
            cache_read: None,
            failures: 0,
            duration_ms: Some(1_000),
            timing: SessionTiming::default(),
            hidden_turns: 0,
            subagent_usage: Some(SubagentUsage {
                calls: 1,
                input: 0,
                output: 0,
                cache_read: 0,
            }),
            rows: Vec::new(),
            ..TrajectoryView::default()
        },
    ))));
    let s = render_usage(&app);
    assert!(s.contains("delegated usage:"), "the row is present: {s}");
    assert!(
        s.contains("usage not reported"),
        "an unmeasured cost says so: {s}"
    );
    assert!(
        !s.contains("0 input"),
        "it does not claim the child spent nothing: {s}"
    );
    let row = s
        .lines()
        .find(|l| l.starts_with("delegated usage:"))
        .expect("the row is present");
    assert!(
        !row.contains("cached"),
        "and no share is invented on the delegated row: {row}"
    );
}
