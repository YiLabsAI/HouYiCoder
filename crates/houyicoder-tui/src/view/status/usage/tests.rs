//! Tests for the Usage sub-tab: the token and cache rows, the latency rows
//! read from the session's typed timing summary, and the delegated-work row.

use super::render_usage;
use crate::command::render::format_tokens;
use crate::state::enums::StatusTab;
use crate::state::{Pane, TrajectoryDrill, TrajectoryTurnKey};
use crate::test_harness::{render_text, working_app};
use crate::view::trajectory_pane::TrajectoryDetailView;
use crate::view::trajectory_pane::{
    RecapSpend, SessionTiming, SubagentUsage, TrajectoryLog, TrajectoryView,
};
use houyicoder_protocol::frontend::status::{
    ApprovalAuditView, ModelUsageView, SessionAuditView, TurnAuditView,
};
use houyicoder_protocol::llm::{Usage, UsageProvenance};
use std::sync::Arc;

/// A trajectory source that answers a fixed view and does not page: the Usage
/// tab reads the session's figures from it and never walks.
struct Fixed(Arc<TrajectoryView>);

impl TrajectoryLog for Fixed {
    fn trajectory(&self) -> Arc<TrajectoryView> {
        self.0.clone()
    }
    fn load_older(&self) {}
    fn load_earliest(&self) {}
    fn return_to_tail(&self) {}
    fn request_detail(&self, _drill: &TrajectoryDrill) {}
    fn detail(&self, _key: &TrajectoryTurnKey) -> Arc<TrajectoryDetailView> {
        Arc::new(TrajectoryDetailView::default())
    }
}

/// The Usage tab renders the token counts from the snapshot.
#[test]
fn test_usage_tab_has_tokens() {
    let app = working_app();
    let s = render_usage(&app);
    assert!(s.contains("Input tokens:"), "input row: {s}");
    assert!(s.contains("Output tokens:"), "output row: {s}");
}

/// Reasoning rides on the output row, only when it is above zero. It is a
/// component of the output total, so a separate row made the reader compare
/// two figures to learn how much of the answer was thinking.
#[test]
fn test_usage_tab_shows_reasoning() {
    let mut app = working_app();
    let mut snap = app.snapshot_or_stub();
    snap.cumulative_usage.output_tokens = 4000;
    snap.cumulative_usage.reasoning_tokens = 1500;
    app.sessions.current.capabilities.status.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(
        s.contains("Output tokens:        4k (1.5k thinking)"),
        "the output row carries the thinking share: {s}"
    );
    assert!(
        !s.contains("reasoning:"),
        "no reasoning row of its own: {s}"
    );

    // Hidden when 0.
    let mut app = working_app();
    let mut snap = app.snapshot_or_stub();
    snap.cumulative_usage.output_tokens = 4000;
    snap.cumulative_usage.reasoning_tokens = 0;
    app.sessions.current.capabilities.status.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(!s.contains("thinking"), "no thinking when 0: {s}");
}

/// The Usage tab renders the per-model section only when two or more
/// models share the session; a single model is already covered by the
/// flat rows and a per-model section would just repeat them.
#[test]
fn test_single_omits_per_model() {
    let mut app = working_app();
    let mut snap = app.snapshot_or_stub();
    snap.by_model = vec![ModelUsageView {
        model: "glm-5.2".into(),
        input_tokens: 1000,
        output_tokens: 500,
        ..Default::default()
    }];
    app.sessions.current.capabilities.status.status_cache = Some(snap);
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
    let mut app = working_app();
    let mut snap = app.snapshot_or_stub();
    snap.by_model = vec![
        ModelUsageView {
            model: "qwen3.7-max".into(),
            input_tokens: 1_500_000,
            output_tokens: 400_000,
            cache_read_tokens: 1_100_000,
            cache_write_tokens: 900_000,
            reasoning_tokens: 0,
            provenance: UsageProvenance::ALL_REPORTED,
        },
        ModelUsageView {
            model: "glm-5.2".into(),
            input_tokens: 300_000,
            output_tokens: 100_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 8_000,
            provenance: UsageProvenance::ALL_REPORTED,
        },
    ];
    app.sessions.current.capabilities.status.status_cache = Some(snap);
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
        !qwen.contains("thinking"),
        "qwen thinking 0 omitted: {qwen}"
    );
    assert!(glm.contains("300k input"), "glm compact: {glm}");
    assert!(glm.contains("8k thinking"), "glm thinking shown: {glm}");
}

/// A model whose calls never reported the cache-read field leaves that count
/// at zero without measuring it. The row states neither the count nor a share
/// for it, rather than a 0% that reads as a miss.
#[test]
fn test_per_model_hides_share() {
    let mut app = working_app();
    let mut snap = app.snapshot_or_stub();
    snap.by_model = vec![
        ModelUsageView {
            model: "glm-5.2".into(),
            input_tokens: 300_000,
            output_tokens: 100_000,
            provenance: UsageProvenance {
                input_reported: true,
                cache_read_reported: false,
                cache_write_reported: false,
            },
            ..Default::default()
        },
        ModelUsageView {
            model: "qwen3.7-max".into(),
            input_tokens: 1_000,
            provenance: UsageProvenance::ALL_REPORTED,
            ..Default::default()
        },
    ];
    app.sessions.current.capabilities.status.status_cache = Some(snap);
    let s = render_usage(&app);
    let row = s
        .lines()
        .find(|l| l.contains("glm-5.2"))
        .expect("the model row");
    assert!(
        !row.contains("cached"),
        "an unmeasured count claims nothing: {row}"
    );
    assert!(!row.contains('%'), "and no share: {row}");
    assert!(
        row.contains("300k input"),
        "the tokens it did report stand: {row}"
    );
}

/// A model that reported its cache-read count and reused nothing states the
/// miss, the same way the session's own row does: a reported zero is a
/// measurement, an unreported one is unknown.
#[test]
fn test_per_model_states_miss() {
    let mut app = working_app();
    let mut snap = app.snapshot_or_stub();
    snap.by_model = vec![
        ModelUsageView {
            model: "glm-5.2".into(),
            input_tokens: 300_000,
            output_tokens: 100_000,
            provenance: UsageProvenance::ALL_REPORTED,
            ..Default::default()
        },
        ModelUsageView {
            model: "qwen3.7-max".into(),
            input_tokens: 1_000,
            provenance: UsageProvenance::ALL_REPORTED,
            ..Default::default()
        },
    ];
    app.sessions.current.capabilities.status.status_cache = Some(snap);
    let s = render_usage(&app);
    let row = s
        .lines()
        .find(|l| l.contains("glm-5.2"))
        .expect("the model row");
    assert!(row.contains("0 cached"), "the miss is stated: {row}");
    assert!(row.contains("(0.0%)"), "with the share it measures: {row}");
}

/// A model id wider than the label column still keeps a separating space,
/// so the row never runs the id and token count together.
#[test]
fn test_long_model_id_spacing() {
    let mut app = working_app();
    let mut snap = app.snapshot_or_stub();
    snap.by_model = vec![
        ModelUsageView {
            model: "deepseek-v4-pro-0813".into(),
            input_tokens: 2_000_000,
            ..Default::default()
        },
        ModelUsageView {
            model: "qwen3.7-max".into(),
            input_tokens: 1_000,
            ..Default::default()
        },
    ];
    app.sessions.current.capabilities.status.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(
        s.contains("deepseek-v4-pro-0813: 2m input"),
        "long id keeps a space: {s}"
    );
    let row = s
        .lines()
        .find(|l| l.contains("qwen3.7-max"))
        .expect("the second model row");
    assert!(
        row.contains("qwen3.7-max: ") && row.contains("1k input"),
        "the shorter id keeps its gap too: {row}"
    );
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
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(view))));
    let s = render_usage(&app);
    assert!(s.contains("Model / tool time:"), "work time row: {s}");
    assert!(s.contains("91.2s / 28.4s"), "the split: {s}");
    assert!(s.contains("TTFT:"), "ttft row: {s}");
    assert!(
        s.contains("1.8s avg · 4.8s p95 · 8.2s p99"),
        "percentiles: {s}"
    );
    assert!(s.contains("(612 samples)"), "the sample count: {s}");
    assert!(s.contains("Decode speed:"), "decode row: {s}");
    assert!(s.contains("31.4 tok/s (590 samples)"), "the rate: {s}");
    // The existing rows keep their names and order above the new ones.
    assert!(s.contains("Input tokens:"), "existing rows intact: {s}");
    assert!(
        s.find("Input tokens:").unwrap() < s.find("Model / tool time:").unwrap(),
        "latency rows come after the token rows: {s}"
    );
}

/// The Usage tab prints a latency under a second in milliseconds, so the
/// row cannot read as a session that took no time while the trajectory
/// header reports the same sample as a span of milliseconds.
#[test]
fn test_usage_tab_short_latency() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
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
    }))));
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
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
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
    }))));
    let s = render_usage(&app);
    assert!(!s.contains("TTFT:"), "no ttft row without samples: {s}");
    assert!(!s.contains("Decode speed:"), "no decode row: {s}");
    assert!(!s.contains("Model / tool time:"), "no work-time row: {s}");
}

/// Delegated sub-agent work is reported on its own row, which says the
/// token rows above already include it.
#[test]
fn test_usage_tab_delegated() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
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
            unmeasured: 0,
            input: 812_000,
            output: 41_000,
            cache_read: 755_000,
            provenance: UsageProvenance::ALL_REPORTED,
        }),
        rows: Vec::new(),
        ..TrajectoryView::default()
    }))));
    let s = render_usage(&app);
    assert!(s.contains("Subagents:"), "the row is present: {s}");
    assert!(
        s.contains("2 calls · 812k input · 41k output · 755k (93.0% cached)"),
        "the row names each count and the share the complete account supports: {s}"
    );
}

/// Recap spend is reported on its own row, which says the token rows above
/// already include it, and names the time the summaries took.
#[test]
fn test_usage_tab_recap_spend() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
        session_id: "s".into(),
        model: "m".into(),
        total_turns: 1,
        tokens_in: Some(4_100),
        tokens_out: Some(300),
        cache_read: Some(3_100),
        failures: 0,
        duration_ms: Some(1_000),
        timing: SessionTiming::default(),
        hidden_turns: 0,
        recap_usage: Some(RecapSpend {
            calls: 3,
            unmeasured: 0,
            input: 4_100,
            output: 300,
            cache_read: 3_100,
            duration_ms: Some(2_400),
            provenance: UsageProvenance {
                input_reported: true,
                cache_read_reported: true,
                cache_write_reported: false,
            },
        }),
        rows: Vec::new(),
        ..TrajectoryView::default()
    }))));
    let s = render_usage(&app);
    assert!(s.contains("Recap usage:"), "the row is present: {s}");
    assert!(
        s.contains("3 calls · 4.1k input · 300 output · 3.1k (75.6% cached) · 2.4s"),
        "the row names each figure, the time, and the count: {s}"
    );
}

/// A recap whose endpoint reported nothing reads as unmeasured, not as a
/// free request. The count of unmeasured calls and the all-reported identity
/// are the shape production emits, so the row must not print the zeroes the
/// identity would let through as measured figures.
#[test]
fn test_usage_tab_recap_unknown() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
        session_id: "s".into(),
        model: "m".into(),
        total_turns: 1,
        tokens_in: None,
        tokens_out: None,
        cache_read: None,
        failures: 0,
        duration_ms: None,
        timing: SessionTiming::default(),
        hidden_turns: 0,
        recap_usage: Some(RecapSpend {
            calls: 1,
            unmeasured: 1,
            provenance: UsageProvenance::ALL_REPORTED,
            ..RecapSpend::default()
        }),
        rows: Vec::new(),
        ..TrajectoryView::default()
    }))));
    let s = render_usage(&app);
    assert!(
        s.contains("usage not reported (1 call)"),
        "an unmeasured recap says so: {s}"
    );
    assert!(
        !s.contains("0 input"),
        "a figure nobody measured is not stated as zero: {s}"
    );
}

/// A session with no delegation shows no delegated row.
#[test]
fn test_usage_tab_no_delegated() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
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
    }))));
    let s = render_usage(&app);
    assert!(!s.contains("Subagents:"), "no row: {s}");
}

/// The cached row divides only when input was reported. With input it
/// shows the share; without it the count stands alone rather than reading
/// as a cache miss.
#[test]
fn test_usage_cache_share() {
    let mut app = working_app();
    let mut snap = app
        .sessions
        .current
        .capabilities
        .status
        .status_cache
        .take()
        .unwrap_or_default();
    snap.model = "m".into();
    snap.cumulative_usage = Usage {
        input_tokens: 1000,
        total_tokens: 1000,
        cache_read_input_tokens: 500,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
        ..Default::default()
    };
    app.sessions.current.capabilities.status.status_cache = Some(snap.clone());
    let s = render_usage(&app);
    assert!(s.contains("50.0% of input"), "the share is shown: {s}");
    snap.cumulative_usage = Usage {
        cache_read_input_tokens: 500,
        ..Default::default()
    };
    app.sessions.current.capabilities.status.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(s.contains("Cached input"), "the row is present: {s}");
    assert!(s.contains("500"), "the count stands: {s}");
    assert!(!s.contains("% of input"), "no share to divide by: {s}");
}

/// The cached row appears with what the session's calls reported: an
/// unreported zero is unknown and stays off the tab, while a zero the
/// endpoints did report is a measured miss the row states.
#[test]
fn test_cached_row_follows_report() {
    let mut app = working_app();
    let mut snap = app
        .sessions
        .current
        .capabilities
        .status
        .status_cache
        .take()
        .unwrap_or_default();
    snap.model = "m".into();
    snap.cumulative_usage = Usage {
        input_tokens: 1000,
        total_tokens: 1000,
        ..Default::default()
    };
    app.sessions.current.capabilities.status.status_cache = Some(snap.clone());
    let s = render_usage(&app);
    assert!(
        !s.contains("Cached input"),
        "no call reported the field, so the tab claims nothing: {s}"
    );
    snap.cumulative_usage.provenance = UsageProvenance {
        input_reported: true,
        cache_read_reported: true,
        cache_write_reported: false,
    };
    app.sessions.current.capabilities.status.status_cache = Some(snap);
    let s = render_usage(&app);
    assert!(
        s.contains("Cached input"),
        "a reported zero is a miss the row states: {s}"
    );
    assert!(
        s.contains("(0.0% of input)"),
        "with the share it measures: {s}"
    );
}

/// A child that returned before reporting usage leaves the row saying so,
/// rather than printing zeroes for a cost that was never measured.
#[test]
fn test_usage_tab_delegated_unreported() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
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
            unmeasured: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            provenance: UsageProvenance::default(),
        }),
        rows: Vec::new(),
        ..TrajectoryView::default()
    }))));
    let s = render_usage(&app);
    assert!(s.contains("Subagents:"), "the row is present: {s}");
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
        .find(|l| l.starts_with("Subagents:"))
        .expect("the row is present");
    assert!(
        !row.contains("cached"),
        "and no share is invented on the delegated row: {row}"
    );
}

/// A child that reported its input total but not its cache-read count leaves
/// the cache-read total at zero without measuring it. The row shows the tokens
/// it did report and no share at all, rather than a 0% that reads as a miss.
#[test]
fn test_delegated_row_cache_unknown() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
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
            unmeasured: 0,
            input: 812_000,
            output: 41_000,
            cache_read: 0,
            provenance: UsageProvenance {
                input_reported: true,
                cache_read_reported: false,
                cache_write_reported: false,
            },
        }),
        rows: Vec::new(),
        ..TrajectoryView::default()
    }))));
    let s = render_usage(&app);
    assert!(s.contains("812k input"), "the reported tokens stand: {s}");
    let row = s
        .lines()
        .find(|l| l.starts_with("Subagents:"))
        .expect("the row is present");
    assert!(
        !row.contains("cached"),
        "an unmeasured cache-read count has no share to show: {row}"
    );
}

/// A child whose cache-read count exceeds its input total would put the share
/// above one. The row holds it to the range the share is defined over, the
/// same way the session row does.
#[test]
fn test_delegated_share_clamped() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
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
            unmeasured: 0,
            input: 100_000,
            output: 4_000,
            cache_read: 150_000,
            provenance: UsageProvenance::ALL_REPORTED,
        }),
        rows: Vec::new(),
        ..TrajectoryView::default()
    }))));
    let s = render_usage(&app);
    assert!(
        s.contains("100.0% cached"),
        "a delegated share with no miss reads as complete: {s}"
    );
}

/// A child that reported only its cache-read count still has that count worth
/// naming. The row names it and leaves out the fields no child measured,
/// rather than reporting the whole child as unmeasured or printing a zero for
/// a field the endpoint never sent.
#[test]
fn test_delegated_row_keeps_cache() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
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
            unmeasured: 0,
            input: 0,
            output: 0,
            cache_read: 12_300,
            provenance: UsageProvenance {
                input_reported: false,
                cache_read_reported: true,
                cache_write_reported: false,
            },
        }),
        rows: Vec::new(),
        ..TrajectoryView::default()
    }))));
    let s = render_usage(&app);
    let row = s
        .lines()
        .find(|l| l.starts_with("Subagents:"))
        .expect("the row is present");
    assert!(
        row.contains("12.3k cached"),
        "the count the child reported is named: {row}"
    );
    assert!(
        !row.contains("usage not reported"),
        "a child that reported something is not called unmeasured: {row}"
    );
    assert!(
        !row.contains("0 input"),
        "a field no child measured is left out, not read as zero: {row}"
    );
    assert!(
        !row.contains('%'),
        "no share over an account the children did not complete: {row}"
    );
}

/// A delegated account whose endpoint sent the input total and omitted the
/// completion total leaves the output count unmeasured. The row names the
/// input it has and says nothing about output, rather than stating a zero no
/// endpoint sent.
#[test]
fn test_delegated_row_omits_output() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
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
            unmeasured: 0,
            input: 900,
            output: 0,
            cache_read: 0,
            provenance: UsageProvenance {
                input_reported: true,
                cache_read_reported: false,
                cache_write_reported: false,
            },
        }),
        rows: Vec::new(),
        ..TrajectoryView::default()
    }))));
    let s = render_usage(&app);
    let row = s
        .lines()
        .find(|l| l.starts_with("Subagents:"))
        .expect("the row is present");
    assert!(
        row.contains("900 input"),
        "the input total the child reported is named: {row}"
    );
    assert!(
        !row.contains("0 output"),
        "a completion total no endpoint sent is not stated as zero: {row}"
    );
}

/// A row covering calls of which some wrote no figure states how many it
/// covers, so the sum cannot pass for the whole.
#[test]
fn test_recap_row_names_unmeasured() {
    let mut app = working_app();
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
        session_id: "s".into(),
        model: "m".into(),
        total_turns: 1,
        timing: SessionTiming::default(),
        recap_usage: Some(RecapSpend {
            calls: 3,
            unmeasured: 1,
            input: 4_100,
            output: 300,
            cache_read: 0,
            duration_ms: Some(2_400),
            provenance: UsageProvenance::default(),
        }),
        ..TrajectoryView::default()
    }))));
    let s = render_usage(&app);
    assert!(
        s.contains("2 of 3 calls measured · 4.1k input · 300 output · 2.4s"),
        "the row states the floor its figures stand for: {s}"
    );
}

/// A session with a full account still draws the recap row inside the pane
/// body: the split of the totals sits beside them, so it stays on screen on a
/// common terminal where the latency rows below it do not fit.
#[test]
fn test_recap_row_stays_visible() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Usage;
    let mut snap = app.snapshot_or_stub();
    snap.cumulative_usage = Usage {
        input_tokens: 1_000_000,
        output_tokens: 40_000,
        reasoning_tokens: 5_000,
        cache_read_input_tokens: 800_000,
        cache_write_input_tokens: 20_000,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: true,
        },
        ..Default::default()
    };
    app.sessions.current.capabilities.status.status_cache = Some(snap);
    app.sessions.persistence.trajectory_log = Some(Arc::new(Fixed(Arc::new(TrajectoryView {
        session_id: "s".into(),
        model: "m".into(),
        total_turns: 2,
        timing: SessionTiming {
            ttft_samples: 4,
            ttft_avg_ms: Some(1_200),
            ttft_p95_ms: Some(2_400),
            ttft_p99_ms: Some(2_600),
            decode_samples: 4,
            decode_tok_per_sec: Some(48.2),
            model_ms: 20_000,
            tool_ms: 5_000,
        },
        subagent_usage: Some(SubagentUsage {
            calls: 2,
            unmeasured: 0,
            input: 812_000,
            output: 41_000,
            cache_read: 755_000,
            provenance: UsageProvenance::ALL_REPORTED,
        }),
        recap_usage: Some(RecapSpend {
            calls: 3,
            unmeasured: 0,
            input: 4_100,
            output: 300,
            cache_read: 3_100,
            duration_ms: Some(2_400),
            provenance: UsageProvenance {
                input_reported: true,
                cache_read_reported: true,
                cache_write_reported: false,
            },
        }),
        ..TrajectoryView::default()
    }))));

    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("Recap usage:"),
        "the row is on screen: {screen}"
    );
    assert!(
        screen.contains("3 calls"),
        "the count is on screen: {screen}"
    );
    assert!(
        screen.contains("4.1k input"),
        "the figures are on screen: {screen}"
    );
}

/// The audit rows state how many approval cards the session drew, the
/// scope each yes bought, and how many of the turns it opened reached
/// their end.
#[test]
fn test_usage_tab_shows_audit() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Usage;
    let mut snap = app.snapshot_or_stub();
    snap.session_audit = SessionAuditView {
        approvals: ApprovalAuditView {
            cards: 4,
            approved_once: 1,
            approved_session: 1,
            approved_workspace: 1,
            denied: 1,
            unknown_outcome: 0,
        },
        turns: TurnAuditView {
            completed: 9,
            interrupted: 1,
            ..TurnAuditView::default()
        },
    };
    app.sessions.current.capabilities.status.status_cache = Some(snap);

    let screen = render_text(&app, 120, 30);
    assert!(
        screen.contains("4 cards · 3 approved · 1 denied (1 once · 1 session · 1 workspace)"),
        "the card row is on screen: {screen}"
    );
    assert!(
        !screen.contains("no verdict"),
        "every card was answered: {screen}"
    );
    // The turn row is gone entirely. A completed share tells a reader nothing
    // they can act on, and over a log from an older build it divides by a
    // denominator that leaves most turns out, so it reads as a clean run
    // beside a large unknown count. Nothing on this tab needs the row.
    assert!(!screen.contains("Turns:"), "no turn row at all: {screen}");
    assert!(
        !screen.contains("1 interrupted"),
        "and no ending restated elsewhere: {screen}"
    );
}

/// A card the user cancelled left no verdict, so the row states it as such
/// instead of counting it among the denials the user gave.
#[test]
fn test_cancelled_ask_reported() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Usage;
    let mut snap = app.snapshot_or_stub();
    snap.session_audit = SessionAuditView {
        approvals: ApprovalAuditView {
            cards: 1,
            denied: 0,
            unknown_outcome: 1,
            ..ApprovalAuditView::default()
        },
        ..SessionAuditView::default()
    };
    app.sessions.current.capabilities.status.status_cache = Some(snap);

    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("1 card · 0 approved · 1 no verdict"),
        "the cancelled card is stated: {screen}"
    );
    assert!(
        !screen.contains("Turns:"),
        "no turn ended, so the turn row is absent: {screen}"
    );
}

/// The pane clips a row that outgrows the terminal rather than folding it, so
/// the adverse counts come before the scope breakdown: at the smallest
/// common width the reader still learns how many cards were refused and how
/// many never got an answer.
#[test]
fn test_audit_rows_survive_clipping() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Usage;
    let mut snap = app.snapshot_or_stub();
    snap.session_audit = SessionAuditView {
        approvals: ApprovalAuditView {
            cards: 13,
            approved_once: 9,
            approved_session: 1,
            denied: 2,
            unknown_outcome: 1,
            ..ApprovalAuditView::default()
        },
        turns: TurnAuditView {
            completed: 7,
            interrupted: 1,
            unfinished: 1,
            unknown: 1,
        },
    };
    app.sessions.current.capabilities.status.status_cache = Some(snap);

    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("13 cards · 10 approved · 2 denied · 1 no verdict"),
        "the counts lead the row: {screen}"
    );
    assert!(
        !screen.contains("Turns:"),
        "no turn row, so no share over a denominator that omits the unknown turns: {screen}"
    );
    assert!(
        !screen.contains("1 unknown"),
        "and the unknown count is not restated: {screen}"
    );

    let wide = render_text(&app, 120, 24);
    assert!(
        !wide.contains("1 unknown"),
        "a wide terminal does not bring the row back: {wide}"
    );
}

/// A turn ending this build cannot read is not shown at all. The row that
/// used to state it is gone, so a log written before endings were recorded
/// cannot surface the count on this tab in any form.
#[test]
fn test_unknown_endings_show_count() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Usage;
    let mut snap = app.snapshot_or_stub();
    snap.session_audit = SessionAuditView {
        turns: TurnAuditView {
            unknown: 3,
            ..TurnAuditView::default()
        },
        ..SessionAuditView::default()
    };
    app.sessions.current.capabilities.status.status_cache = Some(snap);

    let screen = render_text(&app, 80, 24);
    assert!(
        !screen.contains("unknown"),
        "the unreadable endings are not stated: {screen}"
    );
    assert!(
        !screen.contains("completed"),
        "and no share is computed over them: {screen}"
    );
}

/// A call whose turn ended before its result arrived has no outcome, so the
/// row states it: the parts of the total then add up instead of leaving the
/// reader to wonder where one call went.
#[test]
fn test_tool_row_states_unresolved() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Usage;
    let mut snap = app.snapshot_or_stub();
    snap.tool_calls = 3;
    snap.tool_success = 1;
    snap.tool_errors = 1;
    app.sessions.current.capabilities.status.status_cache = Some(snap);

    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("3 (1 ok · 1 failed · 1 no result)"),
        "the call without a result is stated: {screen}"
    );
}

/// A refused call was answered, so it is stated beside the execution counts
/// and taken out of the unresolved remainder. Leaving it in the remainder
/// would report a decision the user made as a result that went missing.
#[test]
fn test_tool_row_states_refused() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Usage;
    let mut snap = app.snapshot_or_stub();
    snap.tool_calls = 4;
    snap.tool_success = 1;
    snap.tool_errors = 1;
    snap.tool_refused = 1;
    app.sessions.current.capabilities.status.status_cache = Some(snap);

    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("4 (1 ok · 1 failed · 1 refused · 1 no result)"),
        "the refusal is stated and only the unanswered call remains: {screen}"
    );
}

/// A session that answered nothing shows neither row: an empty audit is not
/// a zero the reader has to interpret.
#[test]
fn test_empty_audit_omitted() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Usage;
    let screen = render_text(&app, 80, 24);
    assert!(!screen.contains("Approvals:"), "no rows yet: {screen}");
    assert!(!screen.contains("Turns:"), "no rows yet: {screen}");
}
