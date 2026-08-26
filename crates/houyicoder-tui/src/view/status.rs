//! Phase-adaptive viewport chrome: the Working-mode status bar (tiny mark +
//! progress bar + contextual hint), a 1-line actionable status bar for Focus
//! mode, and a 1-line scroll overlay for Scroll mode. The progress bar lives
//! in the Working status bar and folds into the pane border title in Focus
//! mode. Spec id/title and per-clause divergence live inside the spec pane.

pub mod pane;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::state::{App, Stage, ViewportMode};
use crate::view::logo;

/// Build the three-stage progress bar: design / implement / verify. Each
/// stage is marked done (Green check), current (Cyan dot), or pending (dim
/// hollow dot). Used by the Working header, the Focus pane title fusion, and
/// the Focus status bar.
pub fn progress_bar(app: &App) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, s) in Stage::CHAIN.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let (mark, style) = stage_mark(app.stage, *s);
        spans.push(Span::styled(format!("{} {}", s.label(), mark), style));
    }
    Line::from(spans)
}

/// The progress bar as a plain string (for fusing into a pane border title in
/// Focus mode).
pub fn progress_str(app: &App) -> String {
    progress_bar(app)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// The mark and style for one chain stage given the current stage. Done is a
/// Green check, current is a Cyan filled dot, pending is a dim hollow dot.
fn stage_mark(current: Stage, target: Stage) -> (&'static str, Style) {
    if stage_is_done(current, target) {
        ("\u{2713}", Style::new().fg(Color::Green))
    } else if current == target {
        (
            "\u{25cf}",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    } else {
        ("\u{25cb}", Style::new().fg(Color::DarkGray))
    }
}

/// True when the target stage is complete given the current stage. Done
/// stages are those the chain has moved past.
fn stage_is_done(current: Stage, target: Stage) -> bool {
    use Stage::*;
    let order = |s: Stage| match s {
        Idle => 0,
        Design => 1,
        Implementing => 2,
        Verify => 3,
        Done => 4,
    };
    order(current) > order(target)
}

/// Render the bottom status bar (Working mode). When a runner is wired
/// (agent-chat mode), shows one compact line: model | sandbox | busy/idle |
/// tokens | copy hint. When no runner is wired (stub mode for tests/login),
/// keeps the legacy progress bar + stage hint so existing tests render the
/// same surface.
pub fn draw_status_bar(f: &mut Frame, area: Rect, app: &App) {
    // Approval pending: the modal popup (view::approval) is the sole approval
    // surface; the status bar keeps showing the normal agent bar (mode pill +
    // model + tokens) underneath, so the two no longer duplicate the request.
    if app.session.is_some() {
        draw_agent_status_bar(f, area, app);
        return;
    }
    let hint = stage_hint(app);
    let hint_style = Style::new().fg(Color::DarkGray);
    let mut spans = logo::tiny();
    spans.push(Span::raw(" "));
    spans.extend(progress_bar(app).spans);
    spans.push(Span::raw("  "));
    spans.push(Span::styled(hint, hint_style));
    spans.push(Span::raw(" "));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The agent-chat status bar: model · context% · mode · breaker(when Open).
/// One line, dim base. Reads the live runner snapshot (the same seam /context
/// uses), so context% is the real current-window footprint, not a stale
/// per-run token count; the sandbox static string + the drag-to-select hint
/// are dropped (the latter was clutter; selection is discoverable by trying
/// it). The mode pill is colored by autonomy (plan=blue / default=green /
/// acceptEdits=yellow / auto=orange / bypass=red, high-autonomy states add a
/// warning mark) so the current permission surface is always visible.
fn draw_agent_status_bar(f: &mut Frame, area: Rect, app: &App) {
    // Liveness (thinking/idle/approval) is carried by the spinner row at the
    // transcript tail and the input-border shimmer — not duplicated here.
    let dim = Style::new().fg(Color::DarkGray);
    let mode = app.current_mode();
    let (mode_label, mode_style) = mode_pill(mode);
    let mut left: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled(app.status.model.clone(), dim),
        Span::styled(" · ", dim),
        Span::styled(mode_label, mode_style),
    ];
    let mut right: Option<Line<'static>> = None;
    if let Some(snap) = &app.status_cache {
        if snap.breaker_state.as_deref() == Some("Open") {
            left.push(Span::styled(" · ", dim));
            left.push(Span::styled("breaker Open", Style::new().fg(Color::Red)));
        }
        // Token in/out + cache ratio lived here once; dropped as noise. The
        // status bar carries model + mode + context gauge only. Token
        // totals + cache hit rate stay in the /context view and the
        // /trajectory pane, where they carry detail, not the bar.
        // Right-aligned context gauge (persistent — a right-side
        // footprint read). Before the first run last_input is 0 so
        // 0% — honest, not a stale per-run tally. Colored by load. Above 90%
        // (the red zone) append a /compact hint so the user knows the action
        // to free space — "Context low · Run /compact".
        if snap.context_window > 0 {
            let pct = 100.0 * snap.last_input_tokens as f64 / snap.context_window as f64;
            right = Some(Line::from(Span::styled(
                context_gauge_label(pct),
                context_gauge_color(pct),
            )));
        }
    }
    match right {
        Some(right_line) => {
            // Split the single status row: left fills, right holds the
            // right-aligned gauge so the context read sits at the far edge.
            let cells = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(20)])
                .split(area);
            f.render_widget(Paragraph::new(Line::from(left)), cells[0]);
            f.render_widget(
                Paragraph::new(right_line).alignment(Alignment::Right),
                cells[1],
            );
        }
        None => f.render_widget(Paragraph::new(Line::from(left)), area),
    }
}

/// Color the context gauge by load so a filling window reads at a glance:
/// dim under 70%, yellow to 90%, red + bold above 90% (the danger zone where
/// compaction or a shorter turn is imminent).
fn context_gauge_color(pct: f64) -> Style {
    if pct >= 90.0 {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if pct >= 70.0 {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

/// The context-gauge label. Below 90% it reads "X% context used"; at/above
/// 90% (the red zone) it shortens to "X% used · /compact" so the action to
/// free space is one glance away — the
/// "Context low · Run /compact" hint, layered on houyi's existing color gauge.
fn context_gauge_label(pct: f64) -> String {
    if pct >= 90.0 {
        format!(" {:.0}% used · /compact ", pct)
    } else {
        format!(" {:.0}% context used ", pct)
    }
}

/// The mode pill text and color for the status bar. Two modes: Manual shows
/// the pause glyph, Auto the double-play glyph; both always show (the default
/// Auto shows the auto pill from session start). An unknown wire variant
/// renders as manual (fail-safe).
fn mode_pill(mode: houyicoder_protocol::frontend::permission::PermissionMode) -> (String, Style) {
    use houyicoder_protocol::frontend::permission::PermissionMode as M;
    let (symbol, label, color) = match mode {
        M::Manual => ("\u{23f8} ", "manual", Color::Blue),
        M::Auto => ("\u{23f5}\u{23f5} ", "auto", Color::Indexed(208)),
        // non_exhaustive: unknown wire variant fails safe to manual.
        _ => ("\u{23f8} ", "manual", Color::Blue),
    };
    (
        format!("{symbol}{label} mode on (shift+tab to cycle)"),
        Style::new().fg(color),
    )
}

/// The Focus-mode status bar: the actionable keys for the current pane (what
/// a/r/i do right now, plus the focused item index) followed by the progress
/// bar. The input box is folded into this one line: the user presses a/r, not
/// types.
pub fn draw_focus_status(f: &mut Frame, area: Rect, app: &App) {
    let action = focus_action_hint(app);
    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled(
            action,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    spans.extend(progress_bar(app).spans);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The actionable hint for the Focus-mode status bar, per pane. Follows the
/// per-pane approval keys but includes the focused item index so the user
/// knows where they are without the input box prompt.
fn focus_action_hint(app: &App) -> String {
    match (app.pane, app.stage) {
        (crate::state::Pane::Diff, Stage::Implementing) => {
            let n = app.diff.hunks.len();
            let i = app.diff.focus + 1;
            format!("change {i}/{n}  a=approve r=reject  Up/Down=change")
        }
        (crate::state::Pane::Review, Stage::Verify) => {
            let n = app.review.len();
            let i = app.review.focus + 1;
            format!("finding {i}/{n}  a=approve r=reject i=rework")
        }
        (crate::state::Pane::Verify, Stage::Verify) => {
            if app.verify_result.passed {
                "a=complete  r=rework".to_string()
            } else {
                "checks failed  r=rework".to_string()
            }
        }
        _ => stage_hint(app),
    }
}

/// The Scroll-mode overlay status bar: the current line position in the
/// transcript, plus search and tail hints. Replaces the normal status bar
/// while the user is reading history. One line, Cyan + BOLD. It opens with a
/// SCROLL tag so the mode is unambiguous at a glance.
///
/// In the search view's in-view slash re-search bar (search.input_mode), the
/// same row becomes a less-style input bar: slash + the editable buffer with
/// an inverse cursor cell + commit/cancel hints. No match count is shown
/// while editing -- the bar is a snapshot (decision 5), the re-scan runs on
/// Enter, so a count would be the stale prior-query figure. Swapping the bar
/// into the same row keeps the ScrollBox height stable (no layout shift),
/// like a transcript search bar.
pub fn draw_scroll_status(f: &mut Frame, area: Rect, app: &App) {
    let total = app.transcript_display_rows();
    let top = app.transcript_scroll.top_offset(total);
    let line = top.saturating_add(1);
    let style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let line_widget = if app.search.input_mode {
        let buf = app.search.input.value();
        let cur = app.search.input.cursor();
        let (pre, post) = buf.split_at(cur);
        let cursor_char = post.chars().next().unwrap_or(' ');
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(" SEARCH /", style),
            Span::raw(pre.to_string()),
            Span::styled(
                cursor_char.to_string(),
                Style::new().add_modifier(Modifier::REVERSED),
            ),
            Span::raw(post.to_string()),
            Span::styled("  Enter=re-search Esc=cancel", style),
        ];
        if buf.is_empty() {
            spans.push(Span::styled("  (empty = no match)", style));
        }
        Line::from(spans)
    } else if app.search.active {
        let n = app.search.matches.len();
        let focus = if n == 0 { 0 } else { app.search.focus + 1 };
        let query = app.search.query.trim();
        let text = if app.search_truncated {
            // Honest degrade: the log is over the threshold, so the snapshot
            // is empty -- not a "no match". Tell the user why.
            let mb = (app.snapshot_log_bytes / (1024 * 1024)).max(1);
            format!(
                " SEARCH  {query}  log {mb}MB too large — full search needs disk viewport (not built)  q=exit "
            )
        } else if app.window_mode {
            // Indexing (the G full-scan): show progress instead of the
            // position, so the user sees the build advancing + knows Esc stops
            // it. Follows less's indexing indicator style.
            if app.indexing.get() {
                let total = app.index_total.get();
                let pct = (100 * app.indexed_bytes.get())
                    .checked_div(total)
                    .unwrap_or(0);
                format!(" SEARCH  {query}  indexing… {pct}%  Esc=stop  q=exit ")
            } else {
                // Byte-window mode: position is a byte percentage (global line
                // count is unknowable without reading the whole log -- less
                // shows a percentage for the same reason). window_anchor is the
                // byte where the loaded window starts, divided by the frozen
                // file size. No line/total -- the window rows are one screen,
                // not the whole log.
                let pct = app
                    .window_anchor
                    .checked_mul(100)
                    .and_then(|p| p.checked_div(app.frozen_file_size))
                    .unwrap_or(0);
                let skipped = if app.window_skipped > 0 {
                    format!("  · {} lines skipped", app.window_skipped)
                } else {
                    String::new()
                };
                if n == 0 {
                    format!(" SEARCH  {query}  no match  {pct}%  q=exit ")
                } else {
                    format!(
                        " SEARCH  {query}  {focus}/{n}  {pct}%  n=older N=newer{skipped}  q=exit "
                    )
                }
            }
        } else if n == 0 {
            format!(" SEARCH  {query}  no match  q=exit ")
        } else {
            let skipped = if app.search_skipped > 0 {
                format!("  · {} lines skipped", app.search_skipped)
            } else {
                String::new()
            };
            format!(
                " SEARCH  {query}  {focus}/{n}  line {line}/{total}  n=older N=newer{skipped}  q=exit "
            )
        };
        Line::from(Span::styled(text, style))
    } else {
        Line::from(Span::styled(
            format!(" SCROLL  line {line}/{total}  /=search  Esc=tail "),
            style,
        ))
    };
    f.render_widget(Paragraph::new(line_widget), area);
}

/// The contextual hint for the current stage. Tells the user what action is
/// available right now. Follows the per-stage approval keys.
fn stage_hint(app: &App) -> String {
    match app.stage {
        Stage::Idle => {
            let saved = token_savings_k(app);
            if saved > 0 {
                format!("type a task + Enter  /clear saves ~{saved}k")
            } else {
                "type a task + Enter, or /".to_string()
            }
        }
        Stage::Design => "approve design? [a]".to_string(),
        Stage::Implementing => "a=approve r=reject  Up/Down=change".to_string(),
        Stage::Verify => "verify: a=approve/complete r=rework".to_string(),
        Stage::Done => "done. /clear to start over".to_string(),
    }
}

/// Rough token-savings estimate for the /clear nudge: total rendered
/// transcript chars divided by 4, in thousands. Zero when the transcript is
/// still short so the nudge only appears once it would actually free context.
fn token_savings_k(app: &App) -> u64 {
    let chars: usize = app
        .transcript
        .iter()
        .map(|l| l.render().chars().count())
        .sum();
    let tokens = chars / 4;
    if tokens < 2000 {
        0
    } else {
        ((tokens / 1000).max(1)) as u64
    }
}

/// Convenience: true when the viewport is Focus (used by capability pane
/// title fusion to prepend the progress bar).
pub fn is_focus(app: &App) -> bool {
    app.viewport == ViewportMode::Focus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tiny_mark_two_spans() {
        assert_eq!(logo::tiny().len(), 2);
    }

    #[test]
    fn test_gauge_label_hint() {
        // Below 90%: plain "context used", no /compact hint.
        assert!(
            context_gauge_label(50.0).contains("context used"),
            "no hint below 90: {}",
            context_gauge_label(50.0)
        );
        assert!(
            !context_gauge_label(50.0).contains("/compact"),
            "no /compact hint below 90"
        );
        // At/above 90% (red zone): the /compact hint appears so the action is
        // one glance away.
        let high = context_gauge_label(92.0);
        assert!(high.contains("/compact"), "hint appears at 90+: {high}");
        assert!(high.contains("92"), "percent shown: {high}");
    }

    #[test]
    fn test_progress_str_contains_all() {
        let app = crate::composition::app();
        let s = progress_str(&app);
        assert!(s.contains("design"), "got [{s}]");
        assert!(s.contains("implement"), "got [{s}]");
        assert!(s.contains("verify"), "got [{s}]");
    }

    #[test]
    fn test_for_stage_maps_focus() {
        assert_eq!(
            ViewportMode::for_stage(Stage::Implementing),
            ViewportMode::Focus
        );
        assert_eq!(ViewportMode::for_stage(Stage::Verify), ViewportMode::Focus);
        assert_eq!(ViewportMode::for_stage(Stage::Idle), ViewportMode::Working);
        assert_eq!(
            ViewportMode::for_stage(Stage::Design),
            ViewportMode::Working
        );
        assert_eq!(ViewportMode::for_stage(Stage::Done), ViewportMode::Working);
    }

    #[test]
    fn test_bar_no_effort_badge() {
        use houyicoder_protocol::llm::EffortLevel;
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::composition::app();
        app.status.model = "qwen3.7-max".into();
        app.applied_effort = Some(EffortLevel::High);
        let backend = TestBackend::new(80, 3);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_agent_status_bar(f, f.area(), &app))
            .unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!text.contains("high"), "no effort badge: {text}");
        assert!(text.contains("qwen3.7-max"), "model id shown: {text}");
    }

    #[test]
    fn test_bar_drops_token_noise() {
        // A run reported usage (in + out + cache read). The bar must not
        // surface token arrows or a cache pct: that is noise /context and
        // /trajectory already carry. This exercises the post-run snapshot
        // the pre-run render test never reaches, so the noise path stays
        // guarded against re-introduction.
        use houyicoder_protocol::frontend::status::StatusSnapshot;
        use houyicoder_protocol::llm::Usage;
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::composition::app();
        app.status.model = "test-model".into();
        app.status_cache = Some(StatusSnapshot {
            model: "test-model".into(),
            breaker_state: None,
            cumulative_usage: Usage {
                input_tokens: 16100,
                output_tokens: 4200,
                cache_read_input_tokens: 12000,
                ..Default::default()
            },
            last_input_tokens: 16100,
            context_window: 200_000,
            ..Default::default()
        });
        let backend = TestBackend::new(80, 3);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_agent_status_bar(f, f.area(), &app))
            .unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        for noise in ["↑", "↓", "cache ", "16.1k", "4.2k"] {
            assert!(
                !text.contains(noise),
                "token noise {noise:?} leaked into bar: {text}"
            );
        }
        // The model + context gauge (the kept reads) still appear.
        assert!(text.contains("test-model"), "model id shown: {text}");
        assert!(
            text.contains("context") || text.contains("%"),
            "context gauge missing: {text}"
        );
    }
}
