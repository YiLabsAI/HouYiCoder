//! Phase-adaptive viewport chrome: the Working-mode status bar (tiny mark +
//! progress bar + contextual hint), a 1-line actionable status bar for Focus
//! mode, and a 1-line scroll overlay for Scroll mode. The progress bar lives
//! in the Working status bar and folds into the pane border title in Focus
//! mode. Spec id/title and per-clause divergence live inside the spec pane.

pub mod pane;
mod usage;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use houyicoder_protocol::frontend::permission::PermissionMode;
use houyicoder_protocol::llm::count_visible;

use crate::state::{App, Pane, Stage, ViewportMode};
use crate::view::logo;
use Stage::*;

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
        let (mark, style) = stage_mark(app.workflow.stage, *s);
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
    let order = |s: Stage| match s {
        Idle => 0,
        Design => 1,
        Implementing => 2,
        Verify => 3,
        Done => 4,
    };
    order(current) > order(target)
}

/// Render the bottom status bar (Working mode). When a session is connected
/// (agent-chat mode), shows one compact line: model | sandbox | busy/idle |
/// tokens | copy hint. While disconnected (tests/login), keeps the legacy
/// progress bar + stage hint so existing tests render the same surface.
pub fn draw_status_bar(f: &mut Frame, area: Rect, app: &App) {
    // Approval pending: the modal popup (view::approval) is the sole approval
    // surface; the status bar keeps showing the normal agent bar (mode label +
    // model + tokens) underneath, so the two no longer duplicate the request.
    if app.sessions.current.connection.is_some() {
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
/// One line, dim base. Reads the live runner snapshot, so context% is the
/// current-window footprint rather than a per-run tally. The mode label is
/// colored by autonomy; a high-autonomy mode adds a warning mark.
fn draw_agent_status_bar(f: &mut Frame, area: Rect, app: &App) {
    // Liveness (thinking/idle/approval) is carried by the spinner row at the
    // transcript tail and the input-border shimmer — not duplicated here.
    let dim = Style::new().fg(Color::DarkGray);
    let mode = app.current_mode();
    let (mode_label, mode_style) = mode_label(mode);
    let mut left: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled(app.status_bar_model().to_string(), dim),
        Span::styled(" · ", dim),
    ];
    left.push(Span::styled(mode_label, mode_style));
    if let Some(hint) = agents_hint(app) {
        left.push(Span::styled(" · ", dim));
        left.push(Span::styled(hint, dim));
    }
    let mut right: Option<Line<'static>> = None;
    if let Some(snap) = app.status_snapshot() {
        if snap.breaker_state.as_deref() == Some("Open") {
            left.push(Span::styled(" · ", dim));
            left.push(Span::styled("breaker Open", Style::new().fg(Color::Red)));
        }
        // Right-aligned context gauge: the assembled context, the same
        // measurement the context pane renders, so the two cannot disagree.
        // Hidden until a turn assembles one (a zero would read as an empty
        // window); above 90% the label appends the /compact hint.
        if snap.context_window > 0
            && let Some(used) = snap.context_used_tokens
        {
            let pct = 100.0 * used as f64 / snap.context_window as f64;
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

/// The fleet affordance for the status bar: present while the fleet has
/// running children (completed children stay in the strip through the grace
/// window but do not count as live, so the hint drops once nothing is
/// running). The wording follows what the strip can honor this frame:
/// per-row selection needs visible rows, so a strip collapsed to its summary
/// or dropped by the budget advertises the pane instead. The status bar is
/// the one row that never degrades, which is why the count lives here and
/// not only in the strip.
fn agents_hint(app: &App) -> Option<String> {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    let n = entries.iter().filter(|e| e.completed.is_none()).count();
    if n == 0 {
        return None;
    }
    let label = if n == 1 {
        "1 agent".to_string()
    } else {
        format!("{n} agents")
    };
    Some(match app.ui.frame.layout.fleet_granted.get() {
        0 => format!("{label} · /agents"),
        1 if n > 1 => format!("{label} · /agents to manage"),
        _ => format!("{label} · shift+\u{2191}\u{2193} · enter"),
    })
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

/// The mode label text and color for the status bar. Two modes: Manual shows
/// the pause glyph, Auto the double-play glyph; both always show (the default
/// Auto shows the auto label from session start). An unknown wire variant
/// renders as manual (fail-safe).
fn mode_label(mode: PermissionMode) -> (String, Style) {
    let (symbol, label, color) = match mode {
        PermissionMode::Manual => ("\u{23f8} ", "manual", Color::Blue),
        PermissionMode::Auto => ("\u{23f5}\u{23f5} ", "auto", Color::Indexed(208)),
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
    match (app.ui.navigation.pane, app.workflow.stage) {
        (Pane::Diff, Stage::Implementing) => {
            let n = app.workflow.diff.hunks.len();
            let i = app.workflow.diff.focus + 1;
            format!("change {i}/{n}  a=approve r=reject  Up/Down=change")
        }
        (Pane::Review, Stage::Verify) => {
            let n = app.workflow.review.len();
            let i = app.workflow.review.focus + 1;
            format!("finding {i}/{n}  a=approve r=reject i=rework")
        }
        (Pane::Verify, Stage::Verify) => {
            if app.workflow.verify_result.passed {
                "a=complete  r=rework".to_string()
            } else {
                "checks failed  r=rework".to_string()
            }
        }
        _ => stage_hint(app),
    }
}

/// The Scroll-mode overlay status bar: the current line position plus
/// search and tail hints, opening with a SCROLL tag. In the search view's
/// in-view slash bar the same row becomes a less-style input bar: slash, the
/// editable buffer with an inverse cursor, and commit/cancel hints. No match
/// count while editing -- the re-scan runs on Enter, so a count would be the
/// prior query's.
pub fn draw_scroll_status(f: &mut Frame, area: Rect, app: &App) {
    let top = app.active_scroll().top_offset();
    let total = app.active_scroll().total.get();
    let line = top.saturating_add(1);
    let style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let line_widget = if app.ui.transcript_view.search_input_mode() {
        let buf = app.ui.transcript_view.search_input_value();
        let cur = app.ui.transcript_view.search_input_cursor();
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
    } else if app.ui.transcript_view.search_active() {
        let n = app.ui.transcript_view.search_matches_len();
        let focus = if n == 0 {
            0
        } else {
            app.ui.transcript_view.search_focus() + 1
        };
        let query = app.ui.transcript_view.search_query().trim();
        let text = if app.ui.transcript_view.search_corpus_truncated() {
            // Honest degrade: the log is over the threshold, so the snapshot
            // is empty -- not a "no match". Tell the user why.
            let mb = (app.ui.transcript_view.search_corpus_log_bytes() / (1024 * 1024)).max(1);
            format!(
                " SEARCH  {query}  log {mb}MB too large — full search needs disk viewport (not built)  q=exit "
            )
        } else if app.window_browsing() {
            // Scanning (the G full scan): show progress instead of the
            // position, so the user sees the scan advancing + knows Esc stops
            // it. Follows less's indexing indicator style.
            if app.sessions.current.scan_state.indexing.get() {
                let total = app.sessions.current.scan_state.scan_total.get();
                let pct = (100 * app.sessions.current.scan_state.scanned_bytes.get())
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
                    .window_anchor()
                    .checked_mul(100)
                    .and_then(|p| p.checked_div(app.frozen_file_size()))
                    .unwrap_or(0);
                let skipped = if app.window_skipped() > 0 {
                    format!("  · {} lines skipped", app.window_skipped())
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
            let skipped = if app.ui.transcript_view.search_corpus_skipped() > 0 {
                format!(
                    "  · {} lines skipped",
                    app.ui.transcript_view.search_corpus_skipped()
                )
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
    match app.workflow.stage {
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
        .sessions
        .current
        .transcript
        .lines()
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
    app.ui.navigation.viewport == ViewportMode::Focus
}

#[cfg(test)]
mod tests;
