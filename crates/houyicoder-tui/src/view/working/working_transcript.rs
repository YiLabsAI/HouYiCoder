//! Transcript pane rendering for the working surface, extracted from
//! working.rs so the layout file stays under the size gate. Renders the
//! transcript rows (user / agent / tool results / structured diffs / fold
//! groups / the session checklist) with per-row styling, selection tags,
//! inline search highlighting, Ctrl+O expand, and the structured-diff
//! layout (line-numbered green/red bars + word-level highlights + a dim
//! "..." between hunks).

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::live_rows::build_live_rows;
use crate::records::ToolOutcome;
use crate::state::App;
use crate::state::ViewportMode;
use crate::view::context_view;
use crate::view::markers::{diff_row, styled_row};
use crate::view::spinner::{spinner_line, stall_intensity, stall_intensity_reasoning};

#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
pub(super) fn draw_transcript(f: &mut Frame, area: Rect, app: &App) {
    const PLAIN: u8 = crate::selection::TAG_PLAIN;
    const USER: u8 = crate::selection::TAG_USER;
    const SYSTEM: u8 = crate::selection::TAG_SYSTEM;
    const SPINNER: u8 = crate::selection::TAG_SPINNER;
    const DIFF_ADD: u8 = crate::selection::TAG_DIFF_ADD;
    const DIFF_DEL: u8 = crate::selection::TAG_DIFF_DEL;
    const DIFF_HUNK: u8 = crate::selection::TAG_DIFF_HUNK;
    const DIFF_CTX: u8 = crate::selection::TAG_DIFF_CTX;
    const FOLD: u8 = crate::selection::TAG_FOLD;

    // Slots version: only inputs that affect the cached slot text. Live text /
    // spinner / todos are appended after slots + recomputed every frame (the
    // live cache). bash_progress is baked into the slot text (the elapsed/line
    // chip suffix), so it is in the version hash + rebuilds on a tick, not via
    // the old every-frame agent_busy bypass.
    let slots_version = {
        let mut v = app.transcript_version.get();
        v = v.wrapping_mul(31).wrapping_add(app.agent_busy as u64);
        v = v.wrapping_mul(31).wrapping_add(app.verbose as u64);
        v = v
            .wrapping_mul(31)
            .wrapping_add(set_content_hash(&app.expanded_fold_groups));
        v = v
            .wrapping_mul(31)
            .wrapping_add(set_content_hash(&app.expanded_thinking));
        v = v
            .wrapping_mul(31)
            .wrapping_add(set_content_hash(&app.expanded_results));
        // bash_progress is baked into the slots text (push_line_rows appends
        // the elapsed/line-count suffix to a Tool chip), so the cache must
        // rebuild when it changes -- include its content hash so the cache
        // refreshes on a tick (elapsed_secs is second-granularity, so ~1
        // rebuild/sec during a bash run) instead of the old every-frame
        // || agent_busy bypass that defeated the slots cache during runs.
        v = v
            .wrapping_mul(31)
            .wrapping_add(bash_progress_hash(&app.bash_progress));
        // area.width: the slot text is word-wrapped at the pane width
        // (build_slots_rows). A terminal resize changes the width but bumps
        // no other version input -- without this, the cache holds the old
        // wide lines, and a narrower viewport truncates their tails (content
        // vanishes until something else bumps the version).
        v = v.wrapping_mul(31).wrapping_add(area.width as u64);
        v
    };

    if app.display_rows_version.get() != slots_version {
        let (rows, callids, fold_keys, expanded_group, turn_ids, pre_rendered) =
            build_slots_rows(area, app);
        *app.display_rows_cache.borrow_mut() = rows;
        *app.cached_callids.borrow_mut() = callids;
        *app.cached_fold_keys.borrow_mut() = fold_keys;
        *app.cached_expanded_group.borrow_mut() = expanded_group;
        *app.cached_turn_ids.borrow_mut() = turn_ids;
        *app.cached_pre_rendered.borrow_mut() = pre_rendered;
        app.display_rows_version.set(slots_version);
    }

    // Every frame: combine cached slots rows + fresh live rows (live text,
    // spinner, todos). Live rows are cheap (a few rows at most).
    let total_slots = app.display_rows_cache.borrow().len();
    let live = build_live_rows(area, app, total_slots > 0);

    let cap = area.height as usize;
    app.transcript_scroll.cap.set(cap);
    let total = total_slots + live.rows.len();
    app.transcript_scroll.total.set(total);
    let top = app.transcript_scroll.top_offset(total);

    // Slice the visible window [top, top+cap) over (slots ++ live) WITHOUT
    // cloning the full slots cache — only the viewport rows are cloned.
    let visible: Vec<(u8, String, Option<ToolOutcome>)> = {
        let cache = app.display_rows_cache.borrow();
        visible_window(&cache, &live.rows, total_slots, top, cap)
    };
    let visible_callids: Vec<Option<String>> = {
        let cache = app.cached_callids.borrow();
        visible_window(&cache, &live.callids, total_slots, top, cap)
    };
    let visible_fold_keys: Vec<Option<String>> = {
        let cache = app.cached_fold_keys.borrow();
        visible_window(&cache, &live.fold_keys, total_slots, top, cap)
    };
    let visible_expanded_group: Vec<Option<String>> = {
        let cache = app.cached_expanded_group.borrow();
        visible_window(&cache, &live.expanded_group, total_slots, top, cap)
    };
    let visible_turn_ids: Vec<Option<String>> = {
        let cache = app.cached_turn_ids.borrow();
        visible_window(&cache, &live.turn_ids, total_slots, top, cap)
    };
    let visible_pre: Vec<Option<Line<'static>>> = {
        let cache = app.cached_pre_rendered.borrow();
        visible_window(&cache, &live.pre_rendered, total_slots, top, cap)
    };
    // The six parallel arrays are built by independent visible_window calls
    // over caches that are length-aligned by construction. A future edit that
    // desyncs them (a cache rebuilt with a different count) would make the
    // per-row zip + the selection/copy row-index paths drift or panic. Assert
    // the invariant at the seam -- all six must be length-equal.
    debug_assert_eq!(visible.len(), visible_callids.len());
    debug_assert_eq!(visible.len(), visible_fold_keys.len());
    debug_assert_eq!(visible.len(), visible_expanded_group.len());
    debug_assert_eq!(visible.len(), visible_turn_ids.len());
    debug_assert_eq!(visible.len(), visible_pre.len());
    // Stash the full row set (slots + live) for selection/copy.
    {
        let cache = app.display_rows_cache.borrow();
        let mut all: Vec<(u8, String)> = cache.iter().map(|(t, s, _)| (*t, s.clone())).collect();
        all.extend(live.all_rows);
        *app.last_all_rows.borrow_mut() = all;
    }
    let inner = area;
    // Stash rect + visible rows + callid table for mouse/copy/Ctrl+O.
    app.transcript_rect.set(inner);
    // Stash the transcript pane width so the count path (line_display_rows,
    // called from scroll handlers outside the draw borrow) soft-wraps to the
    // SAME width the render path just used — count == render rows.
    app.last_transcript_width.set(inner.width);
    *app.last_transcript_rows.borrow_mut() =
        visible.iter().map(|(t, s, _)| (*t, s.clone())).collect();
    *app.last_row_callids.borrow_mut() = visible_callids;
    *app.last_row_fold_keys.borrow_mut() = visible_fold_keys;
    *app.last_row_expanded_group.borrow_mut() = visible_expanded_group;
    *app.last_row_turn_ids.borrow_mut() = visible_turn_ids;
    f.render_widget(Block::default().borders(Borders::NONE), area);

    let q = if app.search.active {
        app.search.query.trim().to_ascii_lowercase()
    } else {
        String::new()
    };
    // The focused match's transcript line spans multiple screen rows in
    // verbose mode (a tool-result body expands to many rows, and the query
    // may sit mid-body). Mark the WHOLE range [start, start+rows) current so
    // highlighted_line draws yellow on every query occurrence inside the
    // focused line (it no-ops current=true on rows with no query, so non-query
    // rows in the range stay plain). search.matches keys on transcript line,
    // so all in-line occurrences are the same match.
    let focused_range = if app.search.active && !q.is_empty() {
        app.search.focused_line().map(|i| {
            let start = app.transcript_row_of_line(i);
            let rows = app.line_display_rows(&app.active_transcript()[i]);
            start..start + rows
        })
    } else {
        None
    };
    let user_bg = Style::new().bg(Color::Indexed(238));
    let dim = Style::new().fg(Color::DarkGray);
    let exp_grp = app.last_row_expanded_group.borrow();
    let exp_groups = &app.expanded_fold_groups;
    let spin_elapsed = app.run_started.map(|t| t.elapsed()).unwrap_or_default();
    // A running tool produces no token deltas but is not a stall: exempt it
    // from the stall gradient; its presence drives the breathing pulse.
    let tool_active = !app.running_tools.is_empty();
    // Reasoning (LiveBlock::Thinking) also has a sparse token cadence —
    // thinking tokens arrive slowly even when the model is hard at work, so
    // the default 3s stall threshold would flag healthy long thinking as
    // stuck and turn the glyph red. Use the reasoning threshold (10s) so a
    // 30s think stays calm; a true hang still trips once it exceeds it.
    let reasoning_active = app.live_block == crate::state::enums::LiveBlock::Thinking;
    let spin_intensity = if tool_active {
        0.0
    } else if reasoning_active {
        stall_intensity_reasoning(app.last_delta_at)
    } else {
        stall_intensity(app.last_delta_at)
    };
    let lines: Vec<Line> = visible
        .iter()
        .zip(visible_pre)
        .enumerate()
        .map(|(idx, ((tag, r, o), pre))| {
            // current when this screen row falls inside the focused match's
            // multi-row range (top + idx is in folded-display space, matching
            // the range transcript_row_of_line + line_display_rows built).
            let row = top + idx;
            let is_current = focused_range
                .as_ref()
                .is_some_and(|rng| rng.start <= row && row < rng.end);
            // A row inside an expanded fold block carries that block's group
            // key here; paint it with the gray block bg so the whole expanded
            // region reads as one selectable/collapsible affordance (matches
            // an editor's expanded-block selection region).
            let in_expanded = exp_grp
                .get(idx)
                .and_then(|f| f.as_ref())
                .map(|k| exp_groups.contains(k))
                .unwrap_or(false);
            match pre {
                Some(line) => {
                    if q.is_empty() {
                        line
                    } else {
                        highlighted_line(r, &q, is_current)
                    }
                }
                None => match *tag {
                    SPINNER => spinner_line(r, spin_elapsed, spin_intensity, tool_active),
                    USER => highlighted_line(r, &q, is_current).style(user_bg),
                    SYSTEM => highlighted_line(r, &q, is_current).style(if in_expanded {
                        user_bg
                    } else {
                        dim
                    }),
                    FOLD => highlighted_line(r, &q, is_current).style(if in_expanded {
                        user_bg
                    } else {
                        dim
                    }),
                    DIFF_ADD | DIFF_DEL | DIFF_HUNK | DIFF_CTX => {
                        diff_row(r, *tag, inner.width, None)
                    }
                    _ => {
                        let base = if q.is_empty() {
                            styled_row(r, *o).unwrap_or_else(|| highlighted_line(r, &q, is_current))
                        } else {
                            highlighted_line(r, &q, is_current)
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

    // "Jump to bottom" pill: a dim centered overlay on the transcript's
    // bottom row when the user has scrolled back from the tail. Clicking it
    // (hit-tested before the transcript surface in handle_mouse) returns to
    // the tail. A new-messages pill: "N new messages"
    // when agent response segments landed since the scroll-away snapshot,
    // else "Jump to bottom". Hidden while following the tail, while a search
    // or queue overlay is open, and in Scroll mode (which has its own status
    // bar advertising Esc=tail — two "back to bottom" prompts would clash).
    // The hit rect is the centered label span, not the full row — a click on
    // the blank cells to either side must fall through to the transcript
    // surface (start a drag-select), not get swallowed into a jump.
    let pill_visible = app.viewport != ViewportMode::Scroll
        && !app.transcript_scroll.follow_tail
        && !app.search.active
        && !app.queue_view_open;
    if pill_visible {
        let count = app.jump_pill_new_count();
        let label = if count > 0 {
            format!(
                " {count} new message{} ↓",
                if count == 1 { "" } else { "s" }
            )
        } else {
            " Jump to bottom ↓".to_string()
        };
        let row = inner.y.saturating_add(inner.height.saturating_sub(1));
        let label_w = (label.chars().count() as u16).min(inner.width);
        let x = inner.x + (inner.width - label_w) / 2;
        let pill = Rect::new(x, row, label_w, 1);
        f.render_widget(Clear, pill);
        // Bright text on the user-message bg so the pill reads as a clickable
        // affordance (a user-message-background pill,
        // rather than dim text that vanishes into the transcript).
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                label,
                Style::new().fg(Color::White).bg(Color::Indexed(238)),
            ))),
            pill,
        );
        app.jump_pill_rect.set(pill);
    } else {
        app.jump_pill_rect.set(Rect::new(0, 0, 0, 0));
    }
}

/// Emit the rendered rows for one transcript line (the row layer). Shared
/// by the slot-based live render (draw_transcript) and the flat window render
/// (draw_flat_transcript): the slot layer (fold grouping + collapse handles)
/// is the caller's job -- this only does per-line rendering + the leading
/// spacer. The flat window view calls this with grp = None (no fold), so it
/// never touches display_slots/TranscriptScroll/total. Returns false for a
/// Thinking line (emits nothing); true otherwise.
#[expect(clippy::too_many_arguments, reason = "row builder params")]
#[expect(clippy::too_many_lines, reason = "row formatting")]
pub(crate) fn push_line_rows(
    line: &crate::records::TranscriptLine,
    grp: Option<&str>,
    width: u16,
    app: &App,
    rows: &mut Vec<(u8, String, Option<ToolOutcome>)>,
    row_callids: &mut Vec<Option<String>>,
    fold_keys: &mut Vec<Option<String>>,
    expanded_group: &mut Vec<Option<String>>,
    turn_ids: &mut Vec<Option<String>>,
    pre_rendered: &mut Vec<Option<Line<'static>>>,
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
    if !rows.is_empty() && !matches!(line, TranscriptLine::Interrupted) {
        rows.push((PLAIN, String::new(), None));
        row_callids.push(None);
        fold_keys.push(None);
        expanded_group.push(grp_key.clone());
        turn_ids.push(None);
        pre_rendered.push(None);
    }
    if let TranscriptLine::Tool { name, call_id, .. } = line
        && name.as_str() == "result"
    {
        let (body, is_diff) = line.result_body();
        let expanded = app.expanded_results.contains(call_id.as_str()) || app.verbose;
        for (tag, text, oc, cid, word) in app
            .render_cache
            .borrow_mut()
            .tool_rows(&body, call_id, outcome, expanded, is_diff, width)
        {
            rows.push((tag, text.clone(), oc));
            row_callids.push(cid);
            fold_keys.push(None);
            expanded_group.push(grp_key.clone());
            turn_ids.push(None);
            let pre = if matches!(tag, DIFF_ADD | DIFF_DEL | DIFF_HUNK | DIFF_CTX) {
                Some(diff_row(&text, tag, width, word.as_deref()))
            } else {
                None
            };
            pre_rendered.push(pre);
        }
        return true;
    }
    if let TranscriptLine::ContextGrid(view) = line {
        for (plain, styled) in context_view::render_as_rows(view) {
            rows.push((PLAIN, plain, None));
            row_callids.push(None);
            fold_keys.push(None);
            expanded_group.push(grp_key.clone());
            turn_ids.push(None);
            pre_rendered.push(Some(styled));
        }
        return true;
    }
    if let TranscriptLine::Agent(text) = line {
        let (md_lines, md_plain) = app.render_cache.borrow_mut().agent_rows(text, width);
        let mut first = true;
        for (md_line, plain) in md_lines.into_iter().zip(md_plain) {
            let tag = if first {
                crate::selection::TAG_AGENT_FIRST
            } else {
                PLAIN
            };
            rows.push((tag, plain, None));
            row_callids.push(None);
            fold_keys.push(None);
            expanded_group.push(grp_key.clone());
            turn_ids.push(None);
            pre_rendered.push(Some(md_line));
            first = false;
        }
        return true;
    }
    if let TranscriptLine::ThoughtFor {
        secs,
        reasoning,
        turn_id,
        ..
    } = line
    {
        let expanded = app.expanded_thinking.contains(turn_id) || app.verbose;
        let hint = match reasoning {
            Some(_) => {
                if expanded {
                    "collapse"
                } else {
                    "expand"
                }
            }
            None => "",
        };
        let row_text = match hint {
            "" => format!("✻ Thought for {}s", secs),
            _ => format!("✻ Thought for {}s (ctrl+o to {})", secs, hint),
        };
        rows.push((SYSTEM, row_text, None));
        row_callids.push(None);
        fold_keys.push(None);
        expanded_group.push(grp_key.clone());
        turn_ids.push(Some(turn_id.clone()));
        pre_rendered.push(None);
        if let Some(r) = reasoning
            && expanded
        {
            for rline in app.render_cache.borrow_mut().thought_rows(r, width) {
                rows.push((SYSTEM, rline, None));
                row_callids.push(None);
                fold_keys.push(None);
                expanded_group.push(grp_key.clone());
                turn_ids.push(None);
                pre_rendered.push(None);
            }
        }
        return true;
    }
    if let TranscriptLine::User(text) = line {
        for row in app.render_cache.borrow_mut().user_rows(text, width) {
            rows.push((tag, row, outcome));
            row_callids.push(None);
            fold_keys.push(None);
            expanded_group.push(grp_key.clone());
            turn_ids.push(None);
            pre_rendered.push(None);
        }
        return true;
    }
    let text = if app.verbose {
        line.render_verbose()
    } else {
        line.render()
    };
    // Live bash-elapsed suffix: a long-running bash call shows (Ns) on its
    // chip after 2s so a stalled-looking command is distinguishable from a
    // stuck one. When the backend streams stdout, lines Some -> (Ns . M lines).
    let text = if let crate::records::TranscriptLine::Tool { name, call_id, .. } = line {
        if name != "result"
            && let Some(prog) = app.bash_progress.get(call_id)
            && prog.elapsed_secs >= 2
        {
            match prog.lines {
                Some(n) => format!("{text} ({}s · {n} lines)", prog.elapsed_secs),
                None => format!("{text} ({}s)", prog.elapsed_secs),
            }
        } else {
            text
        }
    } else {
        text
    };
    for row in text.split('\n') {
        rows.push((tag, row.to_string(), outcome));
        row_callids.push(None);
        fold_keys.push(None);
        expanded_group.push(grp_key.clone());
        turn_ids.push(None);
        pre_rendered.push(None);
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

type DisplayRows = (
    Vec<(u8, String, Option<ToolOutcome>)>,
    Vec<Option<String>>,
    Vec<Option<String>>,
    Vec<Option<String>>,
    Vec<Option<String>>,
    Vec<Option<Line<'static>>>,
);

/// Order-independent content hash of a string set: XOR each element's stable
/// byte hash so the result does not depend on HashSet iteration order. Replaces
/// the .len() proxy for the slots-version cache key -- .len() collided on
/// same-length different-content sets (expand A, collapse A, expand B -> same
/// length, stale cache). XOR-of-hashes is content-aware; the residual collision
/// (two distinct strings hashing equal) is negligible for small sets + bounded
/// by the next transcript_version bump.
fn set_content_hash(set: &std::collections::HashSet<String>) -> u64 {
    fn str_hash(s: &str) -> u64 {
        s.bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64))
    }
    set.iter().map(|s| str_hash(s)).fold(0u64, |acc, h| acc ^ h)
}

/// Order-independent content hash of the live bash-progress map: XOR each
/// entry's (call_id, elapsed_secs, lines) so the slots-version key catches a
/// bash chip's elapsed/line-count tick (the suffix push_line_rows bakes into
/// the cached slot text). elapsed_secs is second-granularity, so this changes
/// ~once/sec during a bash run -- the cache rebuilds on a tick, not every
/// frame (the old || agent_busy bypass).
fn bash_progress_hash(map: &std::collections::HashMap<String, crate::state::BashProgress>) -> u64 {
    fn str_hash(s: &str) -> u64 {
        s.bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64))
    }
    map.iter()
        .map(|(k, p)| {
            let lines = p.lines.map_or(0, |n| n.wrapping_add(1));
            str_hash(k) ^ (p.elapsed_secs.wrapping_mul(31).wrapping_add(lines))
        })
        .fold(0u64, |acc, h| acc ^ h)
}

/// Build the visible window over slots-then-live without cloning the full
/// slots cache — only the rows the viewport shows are cloned. A
/// virtual scroll: process only what the viewport shows, not the
/// whole transcript. live is already small (streaming text + spinner +
/// todos); the slots cache is what grows unbounded, so the slice-before-clone
/// matters there.
fn visible_window<T: Clone>(
    slots: &[T],
    live: &[T],
    total_slots: usize,
    top: usize,
    cap: usize,
) -> Vec<T> {
    // The real invariant the cross-array asserts at the call site cannot
    // check: total_slots is display_rows_cache.len() (passed in) and is used
    // to bound the slice on ALL six arrays. If a cache length drifts so
    // slots.len() != total_slots, slots[slot_start..slot_end] OOB-panics
    // before the call-site asserts run. Catch it here, with a message, in
    // debug builds -- the slice is the failure surface, not the zip after.
    debug_assert_eq!(
        slots.len(),
        total_slots,
        "total_slots must match this array's length (cache desync)"
    );
    let total = total_slots + live.len();
    let end = top.saturating_add(cap).min(total);
    if top >= end {
        return Vec::new();
    }
    let slot_start = top.min(total_slots);
    let slot_end = end.min(total_slots);
    let live_start = top.saturating_sub(total_slots);
    let live_end = end.saturating_sub(total_slots);
    let mut out: Vec<T> = slots[slot_start..slot_end].to_vec();
    out.extend(live[live_start..live_end].iter().cloned());
    out
}

fn build_slots_rows(area: Rect, app: &App) -> DisplayRows {
    const PLAIN: u8 = crate::selection::TAG_PLAIN;
    const FOLD: u8 = crate::selection::TAG_FOLD;

    let mut rows: Vec<(u8, String, Option<ToolOutcome>)> = Vec::new();
    let mut row_callids: Vec<Option<String>> = Vec::new();
    let mut fold_keys: Vec<Option<String>> = Vec::new();
    let mut expanded_group: Vec<Option<String>> = Vec::new();
    let mut turn_ids: Vec<Option<String>> = Vec::new();
    let mut pre_rendered: Vec<Option<Line<'static>>> = Vec::new();

    let slots = crate::fold::display_slots(
        app.active_transcript(),
        app.agent_busy,
        &app.expanded_fold_groups,
        app.verbose,
    );
    for slot in &slots {
        match slot {
            crate::fold::DisplaySlot::Summary(g) => {
                if !rows.is_empty() {
                    rows.push((PLAIN, String::new(), None));
                    row_callids.push(None);
                    fold_keys.push(None);
                    expanded_group.push(None);
                    turn_ids.push(None);
                    pre_rendered.push(None);
                }
                let sr = crate::fold::render_summary(&g.stats, &g.git_ops, g.active);
                rows.push((FOLD, sr.plain, None));
                row_callids.push(None);
                fold_keys.push(Some(g.key.clone()));
                expanded_group.push(if app.expanded_fold_groups.contains(&g.key) {
                    Some(g.key.clone())
                } else {
                    None
                });
                turn_ids.push(None);
                pre_rendered.push(Some(sr.line));
                if let Some(hint) = g.hint.as_ref() {
                    rows.push((FOLD, format!("  \u{23bf}  {hint}"), None));
                    row_callids.push(None);
                    fold_keys.push(Some(g.key.clone()));
                    expanded_group.push(if app.expanded_fold_groups.contains(&g.key) {
                        Some(g.key.clone())
                    } else {
                        None
                    });
                    turn_ids.push(None);
                    pre_rendered.push(None);
                }
            }
            crate::fold::DisplaySlot::Line(i, grp) => {
                let line = &app.active_transcript()[*i];
                push_line_rows(
                    line,
                    grp.as_deref(),
                    area.width,
                    app,
                    &mut rows,
                    &mut row_callids,
                    &mut fold_keys,
                    &mut expanded_group,
                    &mut turn_ids,
                    &mut pre_rendered,
                );
            }
        }
    }

    (
        rows,
        row_callids,
        fold_keys,
        expanded_group,
        turn_ids,
        pre_rendered,
    )
}

#[cfg(test)]
mod tests {
    use crate::state::TranscriptLine;
    use crate::test_support::render_text;
    use crate::test_support::working_app;

    #[test]
    fn test_cache_version_stable_idle() {
        let mut app = working_app();
        app.transcript.push(TranscriptLine::User("hello".into()));
        let v = app.transcript_version.get().wrapping_add(1);
        app.transcript_version.set(v);
        let out1 = render_text(&app, 80, 24);
        assert!(
            app.display_rows_version.get() != u64::MAX,
            "version should be set after first render"
        );
        let out2 = render_text(&app, 80, 24);
        assert_eq!(out1, out2, "second render should match (cache hit)");
    }

    /// The spinner is separated from the transcript above it by a blank row.
    /// The cached transcript rows and the per-frame live rows are built by two
    /// different functions, so the live builder has to be told the transcript
    /// is non-empty; when it is not, the blank row silently disappears and the
    /// spinner butts up against the last user line.
    #[test]
    fn test_spinner_keeps_blank_above() {
        let mut app = working_app();
        app.transcript.push(TranscriptLine::User("hello".into()));
        app.agent_busy = true;
        app.run_started = Some(std::time::Instant::now());
        let out = render_text(&app, 80, 24);
        let rows: Vec<&str> = out.lines().collect();
        let spinner = rows
            .iter()
            .position(|r| r.contains("Working…"))
            .expect("the spinner row should render while the agent is busy");
        assert!(
            rows[spinner - 1].trim().is_empty(),
            "a blank row should separate the transcript from the spinner, got:\n{out}"
        );
        assert!(
            rows[spinner - 2].contains("hello"),
            "the user line should sit directly above that blank row, got:\n{out}"
        );
    }
}
