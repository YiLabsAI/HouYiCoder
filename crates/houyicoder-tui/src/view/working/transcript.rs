//! Main conversation transcript renderer.
//!
//! Builds selectable rows for user, agent, tool, diff, delegation, and
//! checklist content, then applies scrolling, search, and expansion state.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::tail_rows::build_tail_rows;
use crate::records::TranscriptLine;
use crate::rows::row_set::{Row, RowSet};
use crate::rows::slots_key::SlotsKey;
use crate::selection::{
    TAG_DIFF_ADD, TAG_DIFF_CTX, TAG_DIFF_DEL, TAG_DIFF_HUNK, TAG_FOLD, TAG_SPINNER, TAG_SYSTEM,
    TAG_USER,
};
use crate::state::enums::LiveBlock;
use crate::state::{App, ViewportMode};
use crate::toggle_hint::ToggleHint;
use crate::view::context_view;
use crate::view::markers::{diff_row, styled_row};
use crate::view::spinner::{spinner_line, stall_intensity, stall_intensity_reasoning};

fn ensure_cached_slots(area: Rect, app: &App) {
    // The slot rows depend on the source and its revisions, the wrap width,
    // the mode flags, and the bash ticker. SlotsKey names those inputs and
    // compares them one by one; the live text, spinner, and todos are
    // appended after the slots and rebuilt every frame, so they are not part
    // of it.
    let key = SlotsKey::for_frame(app, area.width);
    if !app.ui.renderer.slots.holds(&key) {
        app.ui
            .renderer
            .slots
            .replace(key, build_slots_rows(area, app));
    }
}

pub(super) fn draw_transcript(f: &mut Frame, area: Rect, app: &App) {
    ensure_cached_slots(area, app);

    // Every frame: combine cached slots rows + fresh live rows (live text,
    // spinner, todos). Live rows are cheap (a few rows at most).
    let total_slots = app.ui.renderer.slots.with(|c| c.len());
    let live = build_tail_rows(area, app, total_slots > 0);

    let cap = area.height as usize;
    app.active_scroll().cap.set(cap);
    let total = total_slots + live.len();
    app.active_scroll().total.set(total);
    let top = app.active_scroll().top_offset();

    // One traversal over (slots ++ live) slices the six arrays by the same
    // bounds, so the visible window stays in step by construction.
    let visible = app.ui.renderer.slots.with(|c| c.visible(&live, top, cap));
    // Stash the full row set (slots + live) for selection/copy.
    {
        let mut all = app.ui.renderer.slots.with(|c| c.text_rows());
        all.extend(live.text_rows());
        *app.ui.frame.layout.last_all_rows.borrow_mut() = all;
    }
    let inner = area;
    // Stash rect + visible rows + callid table for mouse/copy/Ctrl+O.
    app.ui.frame.layout.transcript_rect.set(inner);
    // Stash the transcript pane width so the count path (line_display_rows,
    // called from scroll handlers outside the draw borrow) soft-wraps to the
    // SAME width the render path just used — count == render rows.
    app.ui.frame.feedback.transcript_width.set(inner.width);
    *app.ui.frame.layout.last_transcript_rows.borrow_mut() = visible.text_rows();
    app.ui.frame.keys.borrow_mut().replace(&visible);
    f.render_widget(Block::default().borders(Borders::NONE), area);

    render_transcript_lines(f, inner, app, &visible);
    draw_jump_to_bottom(f, inner, app);
}

fn compute_spinner_params(app: &App) -> (std::time::Duration, f32, bool) {
    let spin_elapsed = app.run_started().map(|t| t.elapsed()).unwrap_or_default();
    let prog = app.run_progress();
    let tool_active = prog.is_some_and(|p| p.tools.any_running());
    let reasoning_active = prog.is_some_and(|p| p.live_block == LiveBlock::Thinking);
    let last_delta = prog.and_then(|p| p.last_delta_at);
    let spin_intensity = if tool_active {
        0.0
    } else if reasoning_active {
        stall_intensity_reasoning(last_delta)
    } else {
        stall_intensity(last_delta)
    };
    (spin_elapsed, spin_intensity, tool_active)
}

fn render_transcript_lines(f: &mut Frame, inner: Rect, app: &App, visible: &RowSet) {
    let top = app.active_scroll().top_offset();
    let q = if app.ui.transcript_view.search_active() {
        app.ui
            .transcript_view
            .search_query()
            .trim()
            .to_ascii_lowercase()
    } else {
        String::new()
    };
    let q = q.as_str();
    // The focused match's transcript line spans multiple screen rows in
    // verbose mode (a tool-result body expands to many rows, and the query
    // may sit mid-body). Mark the WHOLE range [start, start+rows) current so
    // highlighted_line draws yellow on every query occurrence inside the
    // focused line (it no-ops current=true on rows with no query, so non-query
    // rows in the range stay plain). search.matches keys on transcript line,
    // so all in-line occurrences are the same match.
    let focused_range = if app.ui.transcript_view.search_active() && !q.is_empty() {
        app.ui.transcript_view.search_focused_line().map(|i| {
            let start = app.transcript_row_of_line(i);
            let rows = app.line_display_rows(&app.active_transcript()[i]);
            start..start + rows
        })
    } else {
        None
    };
    let user_bg = Style::new().bg(Color::Indexed(238));
    let dim = Style::new().fg(Color::DarkGray);
    let keys = app.ui.frame.keys.borrow();
    let exp_grp = keys.expanded_group();
    let exp_groups = &app.ui.transcript_view.expanded.fold_groups;
    let subagents = &app.ui.transcript_view.expanded.subagents;
    let (spin_elapsed, spin_intensity, tool_active) = compute_spinner_params(app);
    let lines: Vec<Line> = visible
        .rows()
        .iter()
        .zip(visible.pre_rendered())
        .enumerate()
        .map(|(idx, ((tag, r, o), pre))| {
            // current when this screen row falls inside the focused match's
            // multi-row range (top + idx is in folded-display space, matching
            // the range transcript_row_of_line + line_display_rows built).
            let row = top + idx;
            let is_current = focused_range
                .as_ref()
                .is_some_and(|rng| rng.start <= row && row < rng.end);
            // A row inside an expanded block carries that block's key here;
            // the gray background makes the whole region read as one
            // collapsible affordance. Both expand sets are consulted -- a
            // delegation keys into its own.
            let in_expanded = exp_grp
                .get(idx)
                .and_then(|f| f.as_ref())
                .map(|k| exp_groups.contains(k) || subagents.contains(k))
                .unwrap_or(false);
            match pre {
                Some(line) => {
                    let base = if q.is_empty() {
                        line.clone()
                    } else {
                        highlighted_line(r, q, is_current)
                    };
                    // Patch, not replace: a pre-styled line's spans carry the
                    // badge color and markdown styling.
                    if in_expanded {
                        base.patch_style(user_bg)
                    } else {
                        base
                    }
                }
                None => match *tag {
                    TAG_SPINNER => spinner_line(r, spin_elapsed, spin_intensity, tool_active),
                    TAG_USER => user_row(r, q, is_current, inner.width),
                    TAG_SYSTEM => highlighted_line(r, q, is_current).style(if in_expanded {
                        user_bg
                    } else {
                        dim
                    }),
                    TAG_FOLD => highlighted_line(r, q, is_current).style(if in_expanded {
                        user_bg
                    } else {
                        dim
                    }),
                    TAG_DIFF_ADD | TAG_DIFF_DEL | TAG_DIFF_HUNK | TAG_DIFF_CTX => {
                        diff_row(r, *tag, inner.width, None)
                    }
                    _ => {
                        let base = if q.is_empty() {
                            styled_row(r, *o).unwrap_or_else(|| highlighted_line(r, q, is_current))
                        } else {
                            highlighted_line(r, q, is_current)
                        };
                        if in_expanded {
                            base.style(user_bg)
                        } else {
                            base
                        }
                    }
                },
            }
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).style(Style::default().fg(Color::Reset).bg(Color::Reset)),
        inner,
    );
    refresh_wide_cells(f, inner);
}

fn draw_jump_to_bottom(f: &mut Frame, inner: Rect, app: &App) {
    // Jump-to-bottom label: a bright centered overlay on the transcript's
    // bottom row while the user is scrolled back, showing the agent turns
    // that landed since the scroll-away snapshot. Clicking it returns to the
    // tail. Hidden while following the tail, with a search or queue overlay
    // open, or in Scroll mode, whose status bar already advertises Esc=tail.
    // The hit rect is the label span, not the full row, so a click beside the
    // label falls through to the transcript surface and starts a drag-select.
    let label_visible = app.ui.navigation.viewport != ViewportMode::Scroll
        && !app.active_scroll().is_following_tail()
        && !app.ui.transcript_view.search_active();
    if label_visible {
        let new = app.new_turn_count();
        let label = if new.count > 0 {
            let floor = if new.is_lower_bound { "+" } else { "" };
            let plural = if new.count == 1 { "" } else { "s" };
            format!(" {}{floor} new message{plural} ↓", new.count)
        } else {
            " Jump to bottom (click) ↓".to_string()
        };
        let row = inner.y.saturating_add(inner.height.saturating_sub(1));
        let label_w = (label.chars().count() as u16).min(inner.width);
        let x = inner.x + (inner.width - label_w) / 2;
        let rect = Rect::new(x, row, label_w, 1);
        f.render_widget(Clear, rect);
        // Bright text on the user-message bg so the label reads as a clickable
        // affordance rather than dim text that vanishes into the transcript.
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                label,
                Style::new().fg(Color::White).bg(Color::Indexed(238)),
            ))),
            rect,
        );
        app.ui.frame.layout.jump_to_bottom_rect.set(rect);
    } else {
        app.ui
            .frame
            .layout
            .jump_to_bottom_rect
            .set(Rect::new(0, 0, 0, 0));
    }
}

fn rendered_tool_rows(
    line: &crate::records::TranscriptLine,
    grp: Option<&str>,
    width: u16,
    app: &App,
) -> Option<Vec<String>> {
    let full_call = grp
        .is_some_and(|key| app.ui.transcript_view.expanded.fold_groups.contains(key))
        || app.ui.verbose;
    let mut rows = line.tool_call_rows(width, full_call)?;
    // Live bash-elapsed suffix: a long-running bash call shows (Ns) on its
    // chip after 2s so a stalled-looking command is distinguishable from a
    // stuck one. When the backend streams stdout, lines Some -> (Ns . M lines).
    let TranscriptLine::Tool { call_id, .. } = line else {
        return Some(rows);
    };
    let Some(prog) = app.run_progress().and_then(|p| p.tools.tick(call_id)) else {
        return Some(rows);
    };
    if prog.elapsed_secs < 2 {
        return Some(rows);
    }
    let suffix = match prog.lines {
        Some(n) => format!(" ({}s · {n} lines)", prog.elapsed_secs),
        None => format!(" ({}s)", prog.elapsed_secs),
    };
    if let Some(last) = rows.last_mut() {
        let suffix_width = unicode_width::UnicodeWidthStr::width(suffix.as_str());
        let keep = (width as usize).saturating_sub(suffix_width);
        *last = format!(
            "{}{}",
            crate::view::line_wrap::truncate_width(last, keep),
            crate::view::line_wrap::truncate_width(&suffix, width as usize)
        );
    }
    Some(rows)
}

/// Emit the rendered rows for one transcript line (the row layer). Shared
/// by the transcript surface and bounded history window. The slot layer
/// (fold grouping + collapse handles)
/// is the caller's job -- this only does per-line rendering + the leading
/// spacer. The flat window view calls this with grp = None (no fold), so it
/// never touches display_slots/TranscriptScroll/total. Returns false for a
/// Thinking line (emits nothing); true otherwise.
#[expect(clippy::too_many_lines, reason = "row formatting")]
pub(crate) fn push_line_rows(
    line: &crate::records::TranscriptLine,
    grp: Option<&str>,
    width: u16,
    app: &App,
    sink: &mut RowSet,
) -> bool {
    use crate::records::TranscriptLine;
    const PLAIN: u8 = crate::selection::TAG_PLAIN;
    const USER: u8 = crate::selection::TAG_USER;
    const SYSTEM: u8 = crate::selection::TAG_SYSTEM;
    const DIFF_ADD: u8 = crate::selection::TAG_DIFF_ADD;
    const DIFF_DEL: u8 = crate::selection::TAG_DIFF_DEL;
    const DIFF_HUNK: u8 = crate::selection::TAG_DIFF_HUNK;
    const DIFF_CTX: u8 = crate::selection::TAG_DIFF_CTX;
    // Thinking is not rendered as a row (folded into the thought-for line
    // below the answer); its content stays for /search. line_display_rows
    // also returns 0 so the count stays in sync.
    if matches!(line, TranscriptLine::Thinking { .. }) {
        return false;
    }
    let tag = match line {
        TranscriptLine::User(_) => USER,
        TranscriptLine::System(_) => SYSTEM,
        TranscriptLine::Interrupted => SYSTEM,
        TranscriptLine::ThoughtFor { .. } => SYSTEM,
        _ => PLAIN,
    };
    let outcome = match line {
        TranscriptLine::Tool { outcome, .. } => Some(*outcome),
        _ => None,
    };
    let grp_key: Option<String> = grp.map(|g| g.to_string());
    // No spacer before the interrupt notice: it is a child row of the message
    // above, so a blank line between them would read as a separate utterance.
    if !sink.is_empty() && !matches!(line, TranscriptLine::Interrupted) {
        sink.push(Row::spacer().group(grp_key.clone()));
    }
    if let TranscriptLine::Tool { name, call_id, .. } = line
        && name.as_str() == "result"
    {
        let (body, is_diff) = line.result_body();
        let results = &app.ui.transcript_view.expanded.results;
        let expanded = results.contains(call_id.as_str()) || app.ui.verbose;
        for (tag, text, oc, cid, word) in app
            .ui
            .renderer
            .lines
            .borrow_mut()
            .tool_rows(&body, call_id, outcome, expanded, is_diff, width)
        {
            let pre = if matches!(tag, DIFF_ADD | DIFF_DEL | DIFF_HUNK | DIFF_CTX) {
                Some(diff_row(&text, tag, width, word.as_deref()))
            } else {
                None
            };
            // Same rule as the reasoning row: a result inside a delegation
            // publishes no call id, so it is not a separate expand target.
            let cid = if sink.in_subagent() { None } else { cid };
            sink.push(
                Row::new(tag, text)
                    .outcome(oc)
                    .callid(cid)
                    .group(grp_key.clone())
                    .pre(pre),
            );
        }
        return true;
    }
    if let TranscriptLine::ContextGrid(view) = line {
        for (plain, styled) in context_view::render_as_rows(view) {
            sink.push(
                Row::new(PLAIN, plain)
                    .group(grp_key.clone())
                    .pre(Some(styled)),
            );
        }
        return true;
    }
    if let TranscriptLine::Subagent {
        child_sid,
        subagent_type,
        summary,
        prompt: _,
        folded_transcript,
        color,
    } = line
    {
        let delegation = super::delegation_rows::Delegation {
            child_sid,
            subagent_type,
            summary,
            folded_transcript,
            color: color.as_deref(),
        };
        super::delegation_rows::push_delegation_rows(&delegation, grp, width, app, sink);
        return true;
    }
    if let TranscriptLine::Agent(text) = line {
        let (md_lines, md_plain) = app.ui.renderer.lines.borrow_mut().agent_rows(text, width);
        let mut first = true;
        for (md_line, plain) in md_lines.into_iter().zip(md_plain) {
            let tag = if first {
                crate::selection::TAG_AGENT_FIRST
            } else {
                PLAIN
            };
            sink.push(
                Row::new(tag, plain)
                    .group(grp_key.clone())
                    .pre(Some(md_line)),
            );
            first = false;
        }
        return true;
    }
    if let TranscriptLine::ThoughtFor {
        reasoning, turn_id, ..
    } = line
    {
        let expanded = app.ui.transcript_view.expanded.thinking.contains(turn_id) || app.ui.verbose;
        // Inside an expanded delegation the reasoning row is shown, not
        // operated: no hint, and no turn id below, so the enclosing block
        // stays the only thing the next toggle acts on.
        let hint = match reasoning {
            Some(_) if sink.in_subagent() => None,
            Some(_) if expanded => Some(ToggleHint::Collapse),
            Some(_) => Some(ToggleHint::Expand),
            None => None,
        };
        let row_text = line.thought_row_text(hint).unwrap_or_default();
        // The handle is the row's toggle identity: it is published exactly
        // when the row offers a toggle, so a click consults the row the draw
        // produced instead of re-deriving the row's shape from its text.
        let handle = hint.map(|_| turn_id.clone());
        sink.push(
            Row::new(SYSTEM, row_text)
                .group(grp_key.clone())
                .turn_id(handle),
        );
        if let Some(r) = reasoning
            && expanded
        {
            for rline in app.ui.renderer.lines.borrow_mut().thought_rows(r, width) {
                sink.push(Row::new(SYSTEM, rline).group(grp_key.clone()));
            }
        }
        return true;
    }
    if let TranscriptLine::User(text) = line {
        for row in app.ui.renderer.lines.borrow_mut().user_rows(text, width) {
            sink.push(Row::new(tag, row).outcome(outcome).group(grp_key.clone()));
        }
        return true;
    }
    if let Some(rows) = rendered_tool_rows(line, grp, width, app) {
        for row in rows {
            sink.push(Row::new(tag, row).outcome(outcome).group(grp_key.clone()));
        }
        return true;
    }
    let text = if app.ui.verbose {
        line.render_verbose()
    } else {
        line.render()
    };
    for logical in text.split('\n') {
        let rows = if matches!(line, TranscriptLine::System(_)) {
            crate::view::line_wrap::wrap_line(logical, width as usize)
        } else {
            vec![logical.to_string()]
        };
        for row in rows {
            sink.push(Row::new(tag, row).outcome(outcome).group(grp_key.clone()));
        }
    }
    true
}

/// Build one display row as a styled line. When query is non-empty, every
/// case-insensitive occurrence is emphasized; the focused match's row
/// (current=true) uses a yellow background so "where I am" is distinct from
/// "other matches" (Cyan + BOLD). The surrounding text inherits the
/// line/paragraph style. An empty query yields one unstyled span.
pub(crate) fn highlighted_line(row: &str, query: &str, current: bool) -> Line<'static> {
    if query.is_empty() {
        return Line::from(Span::raw(row.to_string()));
    }
    let needle = query.to_ascii_lowercase();
    let qlen = needle.len();
    let hit = if current {
        Style::new()
            .bg(Color::Yellow)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = row;
    let mut acc = String::new();
    loop {
        let lower_rest = rest.to_ascii_lowercase();
        let Some(pos) = lower_rest.find(&needle) else {
            acc.push_str(rest);
            break;
        };
        acc.push_str(&rest[..pos]);
        if !acc.is_empty() {
            spans.push(Span::raw(acc.clone()));
            acc.clear();
        }
        spans.push(Span::styled(rest[pos..pos + qlen].to_string(), hit));
        rest = &rest[pos + qlen..];
    }
    if !acc.is_empty() {
        spans.push(Span::raw(acc));
    }
    if spans.is_empty() {
        spans.push(Span::raw(row.to_string()));
    }
    Line::from(spans)
}

/// Render a user row as one complete background band. Explicit padding keeps
/// wide-character continuation cells connected to the band during scrolling.
pub(crate) fn user_row(row: &str, query: &str, current: bool, width: u16) -> Line<'static> {
    let style = Style::new().bg(Color::Indexed(238));
    let mut line = highlighted_line(row, query, current).style(style);
    let used = unicode_width::UnicodeWidthStr::width(row);
    let padding = (width as usize).saturating_sub(used);
    if padding > 0 {
        line.spans.push(Span::styled(" ".repeat(padding), style));
    }
    line
}

/// Refresh writable cells on rows containing wide glyphs.
pub(crate) fn refresh_wide_cells(f: &mut Frame, area: Rect) {
    let buffer = f.buffer_mut();
    for y in area.y..area.y + area.height {
        let has_wide = (area.x..area.x + area.width).any(|x| {
            buffer
                .cell((x, y))
                .is_some_and(|cell| unicode_width::UnicodeWidthStr::width(cell.symbol()) > 1)
        });
        if !has_wide {
            continue;
        }
        for x in area.x..area.x + area.width {
            let previous_is_wide = x > area.x
                && buffer
                    .cell((x - 1, y))
                    .is_some_and(|cell| unicode_width::UnicodeWidthStr::width(cell.symbol()) > 1);
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            let width = unicode_width::UnicodeWidthStr::width(cell.symbol());
            if width > 1 || (cell.symbol() == " " && !previous_is_wide) {
                cell.set_diff_option(ratatui::buffer::CellDiffOption::AlwaysUpdate);
            }
        }
    }
}

fn fold_hint_row(hint: &str, width: u16) -> String {
    const PREFIX: &str = "  \u{23bf}  ";
    let avail = (width as usize).saturating_sub(5);
    let mut lines = hint.lines();
    let first = lines.next().unwrap_or_default();
    let preview = if lines.next().is_some() {
        format!("{first}\u{2026}")
    } else {
        first.to_string()
    };
    format!(
        "{PREFIX}{}",
        crate::view::line_wrap::truncate_width(&preview, avail)
    )
}

fn build_slots_rows(area: Rect, app: &App) -> RowSet {
    const FOLD: u8 = crate::selection::TAG_FOLD;

    let mut sink = RowSet::default();

    let groups = app.active_fold_groups();
    let fold_groups = &app.ui.transcript_view.expanded.fold_groups;
    let slots = crate::fold::display_slots(
        app.active_transcript(),
        groups.as_ref(),
        fold_groups,
        app.ui.verbose,
    );
    for slot in &slots {
        match slot {
            crate::fold::DisplaySlot::Summary(g) => {
                if !sink.is_empty() {
                    sink.push(Row::spacer());
                }
                // A collapsed group's rows carry its key as the enclosing
                // group only while expanded, so the block background paints
                // the expanded region and not the collapsed handle.
                let enclosing = fold_groups.contains(&g.key).then(|| g.key.clone());
                let toggle = ToggleHint::for_group(g.active, enclosing.is_some());
                let sr = crate::fold::render_summary(&g.stats, &g.git_ops, g.active, toggle);
                sink.push(
                    Row::new(FOLD, sr.plain)
                        .fold_key(Some(g.key.clone()))
                        .group(enclosing.clone())
                        .pre(Some(sr.line)),
                );
                if let Some(hint) = g.hint.as_ref() {
                    sink.push(
                        Row::new(FOLD, fold_hint_row(hint, area.width))
                            .fold_key(Some(g.key.clone()))
                            .group(enclosing),
                    );
                }
            }
            crate::fold::DisplaySlot::NoticeCollapsed { key, idx } => {
                super::notice_rows::push_notice_rows(app, *idx, key, false, area.width, &mut sink);
            }
            crate::fold::DisplaySlot::NoticeExpanded { key, idx } => {
                super::notice_rows::push_notice_rows(app, *idx, key, true, area.width, &mut sink);
            }
            crate::fold::DisplaySlot::Line(i, grp) => {
                let line = &app.active_transcript()[*i];
                push_line_rows(line, grp.as_deref(), area.width, app, &mut sink);
            }
        }
    }

    sink
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
