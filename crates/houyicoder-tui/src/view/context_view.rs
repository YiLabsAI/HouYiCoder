//! /context breakdown visualization, rendered INLINE in the transcript flow
//! as plain, selectable rows (not a widget painted over placeholder rows).
//! Layout: a bold "Context Usage" header, a colored grid (left) beside the
//! legend (right) with a 2-col gap, a blank
//! line, drill-down sections (Memory files, Skills, Delegated) each with a
//! bold header plus dim tree-glyph rows, a blank line, then a Suggestions section
//! (warning/info glyphs + bold title + dim detail). Each visual row is emitted
//! as a (plain text, styled line) pair: the plain text carries the selectable,
//! copyable content (the grid glyphs are ordinary characters) and the styled
//! line keeps the category colors, so a drag lifts exactly what the user sees.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::command::render::format_tokens;
use crate::records::ContextFileEntry;
use crate::records::ContextSkillEntry;
use crate::records::ContextSuggestion;
use crate::records::{ContextView, SuggestionSeverity};
use crate::view::line_wrap::wrap_hanging;
use crate::view::skill_origin::cost_label;
use crate::view::skill_origin::display_order;
use crate::view::skill_origin::heading_spans;
use crate::view::skill_origin::heading_text;
use houyicoder_protocol::frontend::context::{
    ContextBreakdown, DelegatedContext, GridCell, build_grid,
};
use unicode_width::UnicodeWidthStr;

/// Midpoint glyph threshold: a cell at least this full draws the filled ball.
const FULL_GLYPH_THRESHOLD: f32 = 0.7;

/// Continuation glyph prefix for the /context block, matching the transcript
/// tool-result continuation style. U+23BF BOTTOM LEFT CORNER.
const CONTINUATION_GLYPH: &str = "\u{23BF}";

/// Left indent for all lines after the header (the grid, legend, drill-down
/// and suggestions sit 5 columns in, aligned under the header text).
const BLOCK_INDENT: usize = 5;

/// Render the /context block as selectable transcript rows. Returns one
/// (plain text, styled line) pair per visual row, in reading order: the
/// header, the grid-plus-legend top section (each row carries the grid glyphs
/// beside the matching legend line), then the drill-down and suggestion
/// sections. The plain text drives selection and copy; the styled line is
/// painted on screen. An empty grid yields a single "no data yet" row.
///
/// Every row is soft-wrapped to the given display width before it is paired
/// with its plain text, so a long suggestion or summary hangs under its own
/// indent instead of being clipped at the terminal edge. The indent is taken
/// out before the wrap and put back on every line, so it costs the same width
/// budget on a continuation as on the first line. The wrap is the transcript's
/// shared word-boundary helper, which keeps this block's row
/// count identical to the rows the draw path emits.
pub fn render_as_rows(view: &ContextView, width: usize) -> Vec<(String, Line<'static>)> {
    let bd = &view.breakdown;
    if bd.grid.is_empty() {
        return wrap_rows(
            vec![Line::from(vec![Span::styled(
                "Context Usage (no data yet)",
                Style::new().fg(Color::DarkGray),
            )])],
            width,
        );
    }
    let pct = if bd.context_window > 0 {
        100.0 * bd.total_tokens as f64 / bd.context_window as f64
    } else {
        0.0
    };
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let indent = " ".repeat(BLOCK_INDENT);
    let mut out: Vec<Line<'static>> = Vec::new();
    // Header row: the continuation glyph plus the bold title.
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(CONTINUATION_GLYPH),
        Span::raw("  "),
        Span::styled("Context Usage", bold),
    ]));
    // Top section: grid glyphs on the left beside the legend on the right.
    // Grid cells are fixed two columns each, so every legend line starts at
    // the same display column (indent + grid width + gap), faithful to the
    // two-column layout.
    let legend = legend_rows(bd, pct);
    // Dynamically expand the grid if the legend is taller than the current grid,
    // guaranteeing that the left grid square block always matches or exceeds the
    // legend's vertical height (never shorter). Only expand when the grid is
    // a real multi-row grid (height >= 2), avoiding expanding single-cell test stubs.
    let grid_storage: Vec<Vec<GridCell>>;
    let (grid_ref, breakpoint): (&[Vec<GridCell>], Option<usize>) =
        if bd.grid.len() >= 2 && bd.grid.len() < legend.len() {
            grid_storage = build_grid(&bd.categories, bd.context_window, 80, Some(legend.len()));
            let total_cells: usize = grid_storage.iter().map(|r| r.len()).sum();
            let bp = match (bd.cache_prefix_tokens, bd.context_window) {
                (Some(prefix), window) if window > 0 && total_cells > 0 => {
                    let idx = (prefix as f64 / window as f64 * total_cells as f64).round() as usize;
                    Some(idx.min(total_cells.saturating_sub(1)))
                }
                _ => bd.cache_breakpoint,
            };
            (&grid_storage, bp)
        } else {
            (&bd.grid, bd.cache_breakpoint)
        };
    let top = grid_ref.len().max(legend.len());
    let cells_per_row = grid_ref.first().map(|r| r.len()).unwrap_or(0);
    let grid_width = cells_per_row * 2;
    for i in 0..top {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(indent.clone())];
        match grid_ref.get(i) {
            // row_start is the flat cell index of this row's first cell, so
            // grid_row_spans can place the cache-breakpoint bar at the right
            // cell across a multi-row grid.
            Some(row) => spans.extend(grid_row_spans(bd, row, i * cells_per_row, breakpoint)),
            None => spans.push(Span::raw(" ".repeat(grid_width))),
        }
        spans.push(Span::raw("  "));
        if let Some(lrow) = legend.get(i) {
            spans.extend(lrow.iter().cloned());
        }
        out.push(Line::from(spans));
    }
    // Drill-down (Memory files, Skills, Delegated) and Suggestions follow,
    // each section preceded by a blank separator row.
    let memory = memory_rows(&view.drill.memory_files);
    let skills = skills_rows(&view.drill.skills);
    let delegated = delegated_rows(bd.delegated.as_ref());
    let storage = cas::cas_rows(bd.cas.as_ref());
    if !memory.is_empty() || !skills.is_empty() || !delegated.is_empty() || !storage.is_empty() {
        out.push(blank_row());
        push_section(&mut out, &indent, memory);
        push_section(&mut out, &indent, skills);
        push_section(&mut out, &indent, delegated);
        push_section(&mut out, &indent, storage);
    }
    let suggestions = suggestions_rows(&view.suggestions);
    if !suggestions.is_empty() {
        out.push(blank_row());
        push_section(&mut out, &indent, suggestions);
    }
    wrap_rows(out, width)
}

/// The display columns a row's leading spaces occupy, so the wrap can spend
/// them from the same budget on every row instead of adding them on top.
fn leading_indent(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .take_while(|span| span.content.chars().all(|c| c == ' '))
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Wrap each row to the available width and pair it with the plain text a
/// selection or copy lifts. The row's leading indent is measured first and
/// handed to the shared hanging wrap, so a continuation costs the indent out
/// of the same width budget instead of adding it on top: a wrapped row can
/// never be wider than the pane.
fn wrap_rows(rows: Vec<Line<'static>>, width: usize) -> Vec<(String, Line<'static>)> {
    let mut out = Vec::new();
    for row in rows {
        let indent = leading_indent(&row).min(width.saturating_sub(1));
        for line in wrap_hanging(row, indent, width) {
            let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            out.push((plain, line));
        }
    }
    out
}

/// A blank separator row between sections.
fn blank_row() -> Line<'static> {
    Line::raw("")
}

/// Prepend the block indent to each content row and append them to out.
fn push_section(out: &mut Vec<Line<'static>>, indent: &str, rows: Vec<Vec<Span<'static>>>) {
    for row in rows {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(indent.to_string())];
        spans.extend(row);
        out.push(Line::from(spans));
    }
}

/// The spans for one grid row: each cell is a colored glyph plus a space. A
/// cell glyph follows the fill rules: free space draws the hollow square,
/// reserved draws the reserved mark, a cell at least FULL_GLYPH_THRESHOLD full
/// draws the filled ball, otherwise the hollow ball.
fn grid_row_spans(
    bd: &ContextBreakdown,
    row: &[GridCell],
    row_start: usize,
    breakpoint: Option<usize>,
) -> Vec<Span<'static>> {
    row.iter()
        .enumerate()
        .map(|(col, sq)| {
            let cat = bd.categories.get(sq.category_idx);
            let color = cat
                .map(|c| Color::Indexed(c.color_hint))
                .unwrap_or(Color::DarkGray);
            let is_free_space_cat = cat.map(|c| c.label == "Free space").unwrap_or(true);
            let is_reserved = cat.map(|c| c.is_reserved).unwrap_or(false);
            let (glyph, style) = if is_reserved {
                ("\u{26DD}", Style::new().fg(color))
            } else if is_free_space_cat {
                ("\u{26F6}", Style::new().fg(Color::DarkGray))
            } else if sq.fullness >= FULL_GLYPH_THRESHOLD {
                ("\u{26C1}", Style::new().fg(color))
            } else if sq.fullness > 0.0 {
                ("\u{26C0}", Style::new().fg(color))
            } else {
                ("\u{26F6}", Style::new().fg(Color::DarkGray))
            };
            // The cache breakpoint sits between cells: cells [0, bp) are the
            // cached prefix, and bp onward is the fresh suffix. Replace the
            // trailing space after cell (bp - 1) with a yellow vertical bar
            // so the marker does not consume or replace any category cell.
            let cell_idx = row_start + col;
            let is_breakpoint_boundary = breakpoint.is_some_and(|bp| bp > 0 && cell_idx + 1 == bp);
            if is_breakpoint_boundary {
                Span::styled(format!("{glyph}\u{2502}"), Style::new().fg(Color::Yellow))
            } else {
                Span::styled(format!("{glyph} "), style)
            }
        })
        .collect()
}

/// The legend column as one span list per row: model plus total share, a
/// blank, an italic header, one row per visible category (symbol colored, the
/// rest dim), the free-space row, and the reserved rows when present.
fn legend_rows(bd: &ContextBreakdown, pct: f64) -> Vec<Vec<Span<'static>>> {
    let dim = Style::new().fg(Color::Gray);
    let dim_dark = Style::new().fg(Color::DarkGray);
    let italic = Style::new()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);
    let cat_pct = |tokens: u32| -> f64 {
        if bd.context_window > 0 {
            100.0 * tokens as f64 / bd.context_window as f64
        } else {
            0.0
        }
    };
    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(vec![Span::styled(
        format!(
            "{} \u{00b7} {}/{} tokens ({:.0}%)",
            bd.model,
            format_tokens(u64::from(bd.total_tokens)),
            format_tokens(u64::from(bd.context_window)),
            pct,
        ),
        dim,
    )]);
    rows.push(vec![Span::raw("")]);
    rows.push(vec![Span::styled("Estimated usage by category", italic)]);
    for cat in &bd.categories {
        if cat.tokens == 0 || cat.is_deferred || cat.is_reserved || cat.label == "Free space" {
            continue;
        }
        rows.push(vec![
            Span::styled("\u{26C1}", Style::new().fg(Color::Indexed(cat.color_hint))),
            Span::styled(format!(" {}: ", cat.label), dim),
            Span::styled(
                format!(
                    "{} tokens ({:.1}%)",
                    format_tokens(u64::from(cat.tokens)),
                    cat_pct(cat.tokens)
                ),
                dim,
            ),
        ]);
    }
    if let Some(free) = bd.categories.iter().find(|c| c.label == "Free space")
        && free.tokens > 0
    {
        rows.push(vec![
            Span::styled("\u{26F6}", dim_dark),
            Span::styled(" Free space: ", dim),
            Span::styled(
                format!(
                    "{} tokens ({:.1}%)",
                    format_tokens(u64::from(free.tokens)),
                    cat_pct(free.tokens)
                ),
                dim,
            ),
        ]);
    }
    for cat in &bd.categories {
        if !cat.is_reserved || cat.tokens == 0 {
            continue;
        }
        rows.push(vec![
            Span::styled("\u{26DD}", Style::new().fg(Color::Indexed(cat.color_hint))),
            Span::styled(format!(" {}: ", cat.label), dim),
            Span::styled(
                format!(
                    "{} tokens ({:.1}%)",
                    format_tokens(u64::from(cat.tokens)),
                    cat_pct(cat.tokens)
                ),
                dim,
            ),
        ]);
    }
    // Cache prefix is the System prompt + Tools token footprint. Hit rate is
    // the provider-reported ratio from cumulative usage.
    if let Some(prefix) = bd.cache_prefix_tokens {
        let mut line = format!("Cache prefix: {} tokens", format_tokens(u64::from(prefix)));
        if let Some(rate) = bd.cache_hit_rate {
            line.push_str(&format!(" \u{00b7} hit rate {:.0}%", rate * 100.0));
        }
        rows.push(vec![Span::styled(line, dim)]);
    }
    // Compacted summary: present only after a compaction produced a checkpoint.
    if let Some(cs) = &bd.compact_summary {
        rows.push(vec![Span::styled(format!("Compacted: {}", cs), dim)]);
    }
    rows
}

/// The memory source rows: a bold header plus dim suffix, then one tree-glyph
/// row per source (label + tokens). A source is a project memory file, the
/// memory index, or the recalled memories. Empty when the request carries no
/// memory.
fn memory_rows(files: &[ContextFileEntry]) -> Vec<Vec<Span<'static>>> {
    if files.is_empty() {
        return Vec::new();
    }
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(vec![
        Span::styled("Memory files", bold),
        Span::styled(" \u{00b7} /memory", dim),
    ]);
    let last = files.len().saturating_sub(1);
    for (i, file) in files.iter().enumerate() {
        let glyph = if i == last { "\u{2514} " } else { "\u{251C} " };
        rows.push(vec![
            Span::styled(glyph, dim),
            Span::styled(format!("{}: ", file.label), dim),
            Span::styled(
                format!("{} tokens", format_tokens(u64::from(file.tokens))),
                dim,
            ),
        ]);
    }
    rows
}

/// The Skills drill-down rows: a bold header plus dim suffix, then one group
/// per discovery origin (the label and scan path the /skills pane shows) with
/// a tree-glyph row per skill. The rows cover every discovered skill in the
/// pane's own order, so the two views name and sort a skill the same way;
/// a skill the listing does not announce reads manual rather than as a token
/// count, because the window carries none of it.
fn skills_rows(skills: &[ContextSkillEntry]) -> Vec<Vec<Span<'static>>> {
    if skills.is_empty() {
        return Vec::new();
    }
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(vec![
        Span::styled("Skills", bold),
        Span::styled(" \u{00b7} /skills", dim),
    ]);
    let ordered = display_order(skills);
    for (i, s) in ordered.iter().enumerate() {
        if i == 0 || ordered[i - 1].source != s.source {
            rows.push(heading_spans(&s.source, dim, dim));
        }
        let last = ordered
            .get(i + 1)
            .map(|next| next.source != s.source)
            .unwrap_or(true);
        let glyph = if last { "\u{2514}" } else { "\u{251C}" };
        rows.push(vec![
            Span::styled(format!("{glyph} "), dim),
            Span::styled(format!("{}: ", s.name), dim),
            Span::styled(cost_label(s), dim),
        ]);
    }
    rows
}

/// The delegated-work rows: a bold header plus dim suffix, then the tokens the
/// children's returns cost in this session's context, how many returns there
/// were, and what the children spent in their own windows. The child figure
/// stays unknown rather than printing a zero when a return reported no input
/// count. Empty when the session delegated nothing.
fn delegated_rows(delegated: Option<&DelegatedContext>) -> Vec<Vec<Span<'static>>> {
    let Some(delegated) = delegated else {
        return Vec::new();
    };
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(vec![
        Span::styled("Agents", bold),
        Span::styled(" \u{00b7} /agents", dim),
    ]);
    let tasks_label = if delegated.returns == 1 {
        "1 completed task".to_string()
    } else {
        format!("{} completed tasks", delegated.returns)
    };
    rows.push(vec![
        Span::styled("\u{251C} ", dim),
        Span::styled("In context: ", dim),
        Span::styled(
            format!(
                "{} tokens ({tasks_label})",
                format_tokens(u64::from(delegated.returned_tokens))
            ),
            dim,
        ),
    ]);
    let child = match delegated.child_tokens {
        Some(tokens) => format!("{} tokens", format_tokens(u64::from(tokens))),
        None => "unknown".to_string(),
    };
    rows.push(vec![
        Span::styled("\u{2514} ", dim),
        Span::styled("Subagent work: ", dim),
        Span::styled(child, dim),
    ]);
    rows
}

/// The Suggestions rows: a bold header, then per suggestion a title row
/// (warning/info glyph + bold title + optional dim savings) and an indented
/// dim detail row. Empty when there are no suggestions.
fn suggestions_rows(suggestions: &[ContextSuggestion]) -> Vec<Vec<Span<'static>>> {
    if suggestions.is_empty() {
        return Vec::new();
    }
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let warn = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let info = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(vec![Span::styled("Suggestions", bold)]);
    for sug in suggestions {
        let glyph_style = match sug.severity {
            SuggestionSeverity::Warning => warn,
            SuggestionSeverity::Info => info,
        };
        let glyph = sug.severity.glyph();
        let mut title = vec![
            Span::styled(format!("{glyph} "), glyph_style),
            Span::styled(sug.title.clone(), bold),
        ];
        if let Some(savings) = sug.savings_tokens {
            title.push(Span::styled(
                format!(" \u{2192} save ~{}", format_tokens(u64::from(savings))),
                dim,
            ));
        }
        rows.push(title);
        rows.push(vec![Span::raw("  "), Span::styled(sug.detail.clone(), dim)]);
    }
    rows
}

/// Flat-text fallback for the /context grid block. The working-surface
/// renderer draws the real multi-row grid+legend widget; this string exists
/// so /search and any path that flattens the line still surfaces the legend
/// text (model, categories, suggestions) for matching.
pub(crate) fn context_grid_text(view: &ContextView) -> String {
    let bd = &view.breakdown;
    let pct = if bd.context_window > 0 {
        100.0 * bd.total_tokens as f64 / bd.context_window as f64
    } else {
        0.0
    };
    let mut s = String::from("\u{23BF} Context Usage\n");
    s.push_str(&format!(
        "  {} \u{00b7} {}/{} tokens ({:.0}%)\n",
        bd.model, bd.total_tokens, bd.context_window, pct,
    ));
    s.push_str("  Estimated usage by category\n");
    for cat in &bd.categories {
        if cat.tokens == 0 || cat.is_deferred || cat.is_reserved || cat.label == "Free space" {
            continue;
        }
        let c_pct = if bd.context_window > 0 {
            100.0 * cat.tokens as f64 / bd.context_window as f64
        } else {
            0.0
        };
        s.push_str(&format!(
            "  {}: {} tokens ({:.1}%)\n",
            cat.label, cat.tokens, c_pct
        ));
    }
    if !view.drill.memory_files.is_empty() {
        s.push_str("  Memory files\n");
        for f in &view.drill.memory_files {
            s.push_str(&format!("  \u{2514} {}: {} tokens\n", f.label, f.tokens));
        }
    }
    if !view.drill.skills.is_empty() {
        s.push_str("  Skills\n");
        let ordered = display_order(&view.drill.skills);
        for (i, k) in ordered.iter().enumerate() {
            if i == 0 || ordered[i - 1].source != k.source {
                s.push_str(&format!("  {}\n", heading_text(&k.source)));
            }
            s.push_str(&format!("  \u{2514} {}: {}\n", k.name, cost_label(k)));
        }
    }
    if !view.suggestions.is_empty() {
        s.push_str("  Suggestions\n");
        for sug in &view.suggestions {
            s.push_str(&format!(
                "  {} {}\n    {}\n",
                sug.severity.glyph(),
                sug.title,
                sug.detail
            ));
        }
    }
    s.trim_end().to_string()
}

pub(crate) mod cas;

#[cfg(test)]
mod tests;
