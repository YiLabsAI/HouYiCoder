//! /status pane content: renders the Status / Config / Usage sub-tabs into
//! the shared Pane template below the transcript tail. The Status tab's field
//! logic is shared with render_status (the String path the stub /status and
//! the unit tests exercise); this module wraps those lines in a sub-tab header
//! + a footer so the pane is a live surface, not a transcript dump. A
//! Settings-modal-style tabbed status minus the Stats tab. The sub-tab
//! cycles with the Tab, Left, or Right keys.
#![allow(clippy::doc_lazy_continuation)]

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::command::render::{
    field, format_tokens, permission_mode_label, render_breaker_line, render_status,
};
use crate::state::{App, enums::StatusTab};
use crate::view::navigation::{key_hint, tab_header};

/// Default height /status asks for: a header + up to ~12 status lines + a
/// footer. Capped at half the main area by draw_command_pane.
pub(crate) const STATUS_PANE_HEIGHT: u16 = 20;
const STATUS_TABS: [(StatusTab, &str); 3] = [
    (StatusTab::Status, "Status"),
    (StatusTab::Config, "Config"),
    (StatusTab::Usage, "Usage"),
];

/// Render the /status content into the Pane inner rect (the closure passed
/// to draw_command_pane). Sub-tab header + the active tab's content + footer.
pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);
    // Sub-tab header: Status / Config / Usage, the active one bold cyan.
    f.render_widget(
        Paragraph::new(tab_header(app.status_tab, &STATUS_TABS)),
        chunks[0],
    );
    // Body: the active tab's content.
    draw_tab_body(f, app.status_tab, chunks[1], app);
    let footer = Paragraph::new(key_hint(&[
        ("Tab/Left/Right", "switch tab"),
        ("Esc", "close"),
    ]));
    f.render_widget(footer, chunks[2]);
}

/// The body of the active sub-tab. Status reuses render_status (the shared
/// String path + its unit-test authority); Config shows the sandbox / mode /
/// model configuration; Usage shows the token breakdown. When the user is
/// editing the session name on the Status tab, the name row is spliced into
/// an editable line with an inverted caret (the rest of the body is static).
fn draw_tab_body(f: &mut Frame, tab: StatusTab, area: Rect, app: &App) {
    let body: String = match tab {
        StatusTab::Status => {
            let snap = app.snapshot_or_stub();
            render_status(
                &snap,
                &app.session_id,
                &app.status.sandbox,
                &app.todos.items,
            )
        }
        StatusTab::Config => render_config(app),
        StatusTab::Usage => render_usage(app),
    };
    let mut lines: Vec<Line<'static>> = body.lines().map(|l| Line::from(l.to_string())).collect();
    // Splice the name row on the Status tab: an inline hint when browsing
    // (e to rename), an editable line with a caret when editing. The name
    // line is the one whose label starts with "Session name"; finding it
    // (rather than assuming a position) stays robust to render_status
    // reordering its rows.
    if tab == StatusTab::Status
        && let Some(idx) = lines.iter().position(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_start()
                .starts_with("Session name")
        })
    {
        match app.status_name_edit.as_ref() {
            // Editing: replace the row with the editable caret line + a hint.
            Some(field) => lines[idx] = name_edit_line(field),
            // Browsing: append a dim "e to rename" hint to the static row.
            None => lines[idx] = name_hint_line(std::mem::take(&mut lines[idx])),
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Append a dim "e to rename" hint to the static name row so the user knows
/// the inline-edit affordance exists without a separate footer line.
fn name_hint_line(mut row: Line<'static>) -> Line<'static> {
    row.spans.push(Span::styled(
        "  (e to rename)".to_string(),
        Style::new().fg(Color::DarkGray),
    ));
    row
}

/// The editable session-name row: the label + the buffer with an inverted
/// caret at the cursor (a grapheme under the cursor, or a trailing space
/// block when the cursor is past the end). Matches the input-bar caret style.
fn name_edit_line(field: &crate::input::InputField) -> Line<'static> {
    // Label matches render::field's "{:<22}" so the caret row does not jump
    // left/right against the static rows when editing starts/stops.
    let label: Span<'static> = Span::raw(format!("{:<22}", "Session name:"));
    let body_style = Style::new().fg(Color::Reset);
    let cursor_style = Style::new().bg(Color::White).fg(Color::Black);
    let text = field.value();
    let cursor = field.cursor().min(text.len());
    let before = &text[..cursor];
    let rest = &text[cursor..];
    let mut spans: Vec<Span<'static>> = vec![label];
    if !before.is_empty() {
        spans.push(Span::styled(before.to_string(), body_style));
    }
    if let Some(g) = rest.graphemes(true).next() {
        spans.push(Span::styled(g.to_string(), cursor_style));
        let after = &rest[g.len()..];
        if !after.is_empty() {
            spans.push(Span::styled(after.to_string(), body_style));
        }
    } else {
        spans.push(Span::styled(" ".to_string(), cursor_style));
    }
    spans.push(Span::styled(
        "  (Enter save · Esc cancel)".to_string(),
        Style::new().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

/// The Config tab: runtime configuration knobs -- model, permission mode,
/// sandbox, breaker, + the settings-file memory toggles. Reads live app
/// state + the snapshot's toggle fields so the tab is a focused config view.
/// Display-only: the user flips a toggle by editing the settings file (the
/// settings file is the source of truth, edited externally, not inline).
fn render_config(app: &App) -> String {
    let f = field;
    let mode = app.current_mode();
    let snap = app.snapshot_or_stub();
    let on_off = |b: bool| if b { "on" } else { "off" };
    let mut s = String::new();
    // Config tab shows the resolved model id (not the tier label) + the
    // applied effort (None = no effort parameter sent, hidden per I8). Both
    // come from the host snapshot, so the tab cannot name a model the session
    // is not running.
    let applied = &app.model_picker.snapshot.applied;
    s.push_str(&f("Model", &applied.id));
    if let Some(effort) = applied.effort {
        s.push_str(&f("effort", effort.label()));
    }
    s.push_str(&f("Permission mode", permission_mode_label(mode)));
    s.push_str(&f("sandbox", &app.status.sandbox));
    s.push_str(&f("breaker", &render_breaker_line(&snap)));
    s.push_str(&f("auto-memory", on_off(snap.auto_memory)));
    s.push_str(&f("auto-dream", on_off(snap.auto_dream)));
    s.trim_end().to_string()
}

#[path = "usage.rs"]
mod usage;
use usage::render_usage;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::trajectory_pane::{
        DelegatedUsage, SessionTiming, TrajectoryLog, TrajectoryView,
    };

    /// The sub-tab header renders Status / Config / Usage, with the active one
    /// marked (the active title appears in the header).
    #[test]
    fn test_header_renders_all_tabs() {
        let line = tab_header(StatusTab::Status, &STATUS_TABS);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("Status"), "Status tab: {rendered}");
        assert!(rendered.contains("Config"), "Config tab: {rendered}");
        assert!(rendered.contains("Usage"), "Usage tab: {rendered}");
    }

    /// The Config tab renders model / permission mode / sandbox / breaker +
    /// the settings-file memory toggles (auto-memory / auto-dream).
    #[test]
    fn test_config_tab_has_knobs() {
        let mut app = crate::test_harness::working_app();
        app.model_picker.snapshot.applied.id = "qwen3.8-max".into();
        app.status.sandbox = "mac-seatbelt".into();
        let s = render_config(&app);
        assert!(s.contains("qwen3.8-max"), "model: {s}");
        assert!(s.contains("mac-seatbelt"), "sandbox: {s}");
        assert!(s.contains("Permission mode:"), "permission mode: {s}");
        assert!(s.contains("breaker:"), "breaker: {s}");
        assert!(s.contains("auto-memory:"), "auto-memory row: {s}");
        assert!(s.contains("auto-dream:"), "auto-dream row: {s}");
    }

    /// The Config tab renders the toggle state from the snapshot, so a snap
    /// with auto-memory off shows "off" (the row reflects the wire value, not
    /// a hardcoded default).
    #[test]
    fn test_config_tab_reflects_state() {
        let mut app = crate::test_harness::working_app();
        let mut snap = app.snapshot_or_stub();
        snap.auto_memory = false;
        snap.auto_dream = true;
        app.status_cache = Some(snap);
        let s = render_config(&app);
        let memory_line = s
            .lines()
            .find(|l| l.trim_start().starts_with("auto-memory"))
            .unwrap_or_else(|| panic!("auto-memory row missing: {s}"));
        let dream_line = s
            .lines()
            .find(|l| l.trim_start().starts_with("auto-dream"))
            .unwrap_or_else(|| panic!("auto-dream row missing: {s}"));
        assert!(memory_line.ends_with("off"), "off toggle: {s}");
        assert!(dream_line.ends_with("on"), "on toggle: {s}");
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

    /// The Config tab renders the resolved model id + the effort badge
    /// (only when the next request really carries an effort parameter).
    #[test]
    fn test_config_tab_shows_effort() {
        use houyicoder_protocol::llm::EffortLevel;
        let mut app = crate::test_harness::working_app();
        app.model_picker.snapshot.applied.id = "qwen3.8-max".into();
        app.model_picker.snapshot.applied.effort = Some(EffortLevel::High);
        let s = render_config(&app);
        assert!(s.contains("effort:"), "effort row: {s}");
        assert!(s.contains("high"), "high level: {s}");
        // Effort row hidden when None.
        app.model_picker.snapshot.applied.effort = None;
        let s = render_config(&app);
        assert!(!s.contains("effort:"), "no effort row when None: {s}");
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

    /// The editable name line renders the label + the buffer text (cursor at
    /// the end renders a trailing space block).
    #[test]
    fn test_name_edit_renders_buffer() {
        let mut field = crate::input::InputField::new();
        field.insert_str("fix");
        let line = name_edit_line(&field);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("Session name: "),
            "label present: {rendered}"
        );
        assert!(rendered.contains("fix"), "buffer text present: {rendered}");
    }

    /// An empty buffer still renders the label + a cursor block (the caret at
    /// end), so the user sees where typing lands even before the first char.
    #[test]
    fn test_name_edit_renders_caret() {
        let field = crate::input::InputField::new();
        let line = name_edit_line(&field);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("Session name: "),
            "label present: {rendered}"
        );
    }

    /// The cursor mid-buffer splits the text: a grapheme under the caret is
    /// styled separately from the before/after text.
    #[test]
    fn test_name_edit_cursor_buffer() {
        let mut field = crate::input::InputField::new();
        field.insert_str("abc");
        field.move_left(); // cursor between b and c (after "ab")
        let line = name_edit_line(&field);
        // Three text spans: "  Session name: ", "ab", the caret char "c"
        // (the cursor sits on 'c' so 'ab' is before + 'c' is the caret).
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("ab"), "before-caret text: {rendered}");
        assert!(rendered.contains('c'), "caret char: {rendered}");
    }

    /// The Usage tab reports session latency from the same typed summary the
    /// trajectory pane reads, and omits a row the session has no sample for.
    #[test]
    fn test_usage_tab_latency_rows() {
        struct Fixed(TrajectoryView);
        impl TrajectoryLog for Fixed {
            fn trajectory(&self) -> TrajectoryView {
                self.0.clone()
            }
        }
        let view = TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 1,
            tokens_in: Some(10),
            tokens_out: Some(5),
            cache_read: None,
            failures: 0,
            duration_secs: 1,
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
            delegated: None,
            rows: Vec::new(),
        };
        let mut app = crate::test_harness::working_app();
        app.trajectory_log = Some(std::sync::Arc::new(Fixed(view)));
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

    /// With no timing recorded, the Usage tab shows no latency row at all: an
    /// unmeasured session must not read as instant.
    #[test]
    fn test_usage_tab_no_timing() {
        struct Fixed(TrajectoryView);
        impl TrajectoryLog for Fixed {
            fn trajectory(&self) -> TrajectoryView {
                self.0.clone()
            }
        }
        let mut app = crate::test_harness::working_app();
        app.trajectory_log = Some(std::sync::Arc::new(Fixed(TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 0,
            tokens_in: None,
            tokens_out: None,
            cache_read: None,
            failures: 0,
            duration_secs: 0,
            timing: SessionTiming::default(),
            hidden_turns: 0,
            delegated: None,
            rows: Vec::new(),
        })));
        let s = render_usage(&app);
        assert!(!s.contains("ttft:"), "no ttft row without samples: {s}");
        assert!(!s.contains("decode speed:"), "no decode row: {s}");
        assert!(!s.contains("model / tool time:"), "no work-time row: {s}");
    }

    /// Delegated sub-agent work is reported on its own row and says it is not
    /// part of the rows above, because those come from the parent's calls only.
    #[test]
    fn test_usage_tab_delegated() {
        struct Fixed(TrajectoryView);
        impl TrajectoryLog for Fixed {
            fn trajectory(&self) -> TrajectoryView {
                self.0.clone()
            }
        }
        let mut app = crate::test_harness::working_app();
        app.trajectory_log = Some(std::sync::Arc::new(Fixed(TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 1,
            tokens_in: Some(10),
            tokens_out: Some(5),
            cache_read: None,
            failures: 0,
            duration_secs: 1,
            timing: SessionTiming::default(),
            hidden_turns: 0,
            delegated: Some(DelegatedUsage {
                calls: 2,
                input: 812_000,
                output: 41_000,
                cache_read: 755_000,
            }),
            rows: Vec::new(),
        })));
        let s = render_usage(&app);
        assert!(s.contains("delegated usage:"), "the row is present: {s}");
        assert!(s.contains("812k input"), "input shown: {s}");
        assert!(s.contains("41k output"), "output shown: {s}");
        assert!(s.contains("93% cached"), "the child cache share: {s}");
        assert!(
            s.contains("not in the rows above"),
            "the row says how it combines with the totals: {s}"
        );
    }

    /// A session with no delegation shows no delegated row.
    #[test]
    fn test_usage_tab_no_delegated() {
        struct Fixed(TrajectoryView);
        impl TrajectoryLog for Fixed {
            fn trajectory(&self) -> TrajectoryView {
                self.0.clone()
            }
        }
        let mut app = crate::test_harness::working_app();
        app.trajectory_log = Some(std::sync::Arc::new(Fixed(TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 1,
            tokens_in: Some(10),
            tokens_out: Some(5),
            cache_read: None,
            failures: 0,
            duration_secs: 1,
            timing: SessionTiming::default(),
            hidden_turns: 0,
            delegated: None,
            rows: Vec::new(),
        })));
        let s = render_usage(&app);
        assert!(!s.contains("delegated usage:"), "no row: {s}");
    }

    /// A child that returned before reporting usage leaves the row saying so,
    /// rather than printing zeroes for a cost that was never measured.
    #[test]
    fn test_usage_tab_delegated_unreported() {
        struct Fixed(TrajectoryView);
        impl TrajectoryLog for Fixed {
            fn trajectory(&self) -> TrajectoryView {
                self.0.clone()
            }
        }
        let mut app = crate::test_harness::working_app();
        app.trajectory_log = Some(std::sync::Arc::new(Fixed(TrajectoryView {
            session_id: "s".into(),
            model: "m".into(),
            total_turns: 1,
            tokens_in: Some(10),
            tokens_out: Some(5),
            cache_read: None,
            failures: 0,
            duration_secs: 1,
            timing: SessionTiming::default(),
            hidden_turns: 0,
            delegated: Some(DelegatedUsage {
                calls: 1,
                input: 0,
                output: 0,
                cache_read: 0,
            }),
            rows: Vec::new(),
        })));
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
}
