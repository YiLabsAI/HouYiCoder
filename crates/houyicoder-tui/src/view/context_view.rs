//! /context breakdown visualization, rendered INLINE in the transcript flow
//! as plain, selectable rows (not a widget painted over placeholder rows).
//! Layout: a bold "Context Usage" header, a colored grid (left) beside the
//! colored grid (left) beside the legend (right) with a 2-col gap, a blank
//! line, drill-down sections (Memory files, Skills) each with a bold header
//! plus dim tree-glyph rows, a blank line, then a Suggestions section
//! (warning/info glyphs + bold title + dim detail). Each visual row is emitted
//! as a (plain text, styled line) pair: the plain text carries the selectable,
//! copyable content (the grid glyphs are ordinary characters) and the styled
//! line keeps the category colors, so a drag lifts exactly what the user sees.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::records::{ContextView, SuggestionSeverity};
use houyicoder_protocol::frontend::context::{ContextBreakdown, GridSquare};

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
pub fn render_as_rows(view: &ContextView) -> Vec<(String, Line<'static>)> {
    let bd = &view.breakdown;
    if bd.grid.is_empty() {
        return vec![row_from_spans(vec![Span::styled(
            "Context Usage (no data yet)",
            Style::new().fg(Color::DarkGray),
        )])];
    }
    let pct = if bd.context_window > 0 {
        100.0 * bd.total_tokens as f64 / bd.context_window as f64
    } else {
        0.0
    };
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let indent = " ".repeat(BLOCK_INDENT);
    let mut out: Vec<(String, Line<'static>)> = Vec::new();
    // Header row: the continuation glyph plus the bold title.
    out.push(row_from_spans(vec![
        Span::raw("  "),
        Span::raw(CONTINUATION_GLYPH),
        Span::raw("  "),
        Span::styled("Context Usage", bold),
    ]));
    // Top section: grid glyphs on the left beside the legend on the right.
    // Grid cells are fixed two columns each, so every legend line starts at
    // the same display column (indent + grid width + gap), faithful to the
    // the two-column layout.
    let legend = legend_rows(bd, pct);
    let top = bd.grid.len().max(legend.len());
    let cells_per_row = bd.grid.first().map(|r| r.len()).unwrap_or(0);
    let grid_width = cells_per_row * 2;
    for i in 0..top {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(indent.clone())];
        match bd.grid.get(i) {
            // row_start is the flat cell index of this row's first cell, so
            // grid_row_spans can place the cache-breakpoint bar at the right
            // cell across a multi-row grid.
            Some(row) => spans.extend(grid_row_spans(bd, row, i * cells_per_row)),
            None => spans.push(Span::raw(" ".repeat(grid_width))),
        }
        spans.push(Span::raw("  "));
        if let Some(lrow) = legend.get(i) {
            spans.extend(lrow.iter().cloned());
        }
        out.push(row_from_spans(spans));
    }
    // Drill-down (Memory files then Skills) and Suggestions follow, each
    // section preceded by a blank separator row.
    let memory = memory_rows(&view.drill.memory_files);
    let skills = skills_rows(&view.drill.skills);
    if !memory.is_empty() || !skills.is_empty() {
        out.push(blank_row());
        push_section(&mut out, &indent, memory);
        push_section(&mut out, &indent, skills);
    }
    let suggestions = suggestions_rows(&view.suggestions);
    if !suggestions.is_empty() {
        out.push(blank_row());
        push_section(&mut out, &indent, suggestions);
    }
    out
}

/// Build one (plain text, styled line) row from an ordered span list. The
/// plain text is the span contents concatenated, so its display columns match
/// the styled line exactly — a drag over the colored grid or legend lifts the
/// text under the pointer.
fn row_from_spans(spans: Vec<Span<'static>>) -> (String, Line<'static>) {
    let plain: String = spans.iter().map(|s| s.content.as_ref()).collect();
    (plain, Line::from(spans))
}

/// A blank separator row between sections.
fn blank_row() -> (String, Line<'static>) {
    (String::new(), Line::raw(""))
}

/// Prepend the block indent to each content row and append the pairs to out.
fn push_section(
    out: &mut Vec<(String, Line<'static>)>,
    indent: &str,
    rows: Vec<Vec<Span<'static>>>,
) {
    for row in rows {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(indent.to_string())];
        spans.extend(row);
        out.push(row_from_spans(spans));
    }
}

/// The spans for one grid row: each cell is a colored glyph plus a space. A
/// cell glyph follows the fill rules: free space draws the hollow square,
/// reserved draws the reserved mark, a cell at least FULL_GLYPH_THRESHOLD full
/// draws the filled ball, otherwise the hollow ball.
fn grid_row_spans(
    bd: &ContextBreakdown,
    row: &[GridSquare],
    row_start: usize,
) -> Vec<Span<'static>> {
    row.iter()
        .enumerate()
        .map(|(col, sq)| {
            // The cache breakpoint is a flat cell index: cells before it are
            // the cached prefix, it onward is the fresh suffix. Draw a vertical
            // bar at the breakpoint cell to mark where the cache ends.
            if bd.cache_breakpoint == Some(row_start + col) {
                return Span::styled("\u{2502} ", Style::new().fg(Color::Yellow));
            }
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
            Span::styled(format!("{glyph} "), style)
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
            fmt_tokens(bd.total_tokens),
            fmt_tokens(bd.context_window),
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
                    fmt_tokens(cat.tokens),
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
                format!("{} ({:.1}%)", fmt_tokens(free.tokens), cat_pct(free.tokens)),
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
                    fmt_tokens(cat.tokens),
                    cat_pct(cat.tokens)
                ),
                dim,
            ),
        ]);
    }
    // Cache prefix is always populated by dispatch (System prompt + Tools
    // tokens). Hit rate needs a prior provider turn (input_tokens > 0); it is
    // None under a stub/zero-turn session, so render it as a suffix only when
    // present — the prefix line still shows without it. Pairing the two
    // (both- Some) would hide the prefix whenever hit rate is absent.
    if let Some(prefix) = bd.cache_prefix_tokens {
        let mut line = format!("Cache prefix: {}", fmt_tokens(prefix));
        if let Some(rate) = bd.cache_hit_rate {
            line.push_str(&format!(" \u{00b7} hit rate {:.0}%", rate * 100.0));
        }
        rows.push(vec![Span::styled(line, dim)]);
    }
    // Folded summary: present only after a compaction produced a checkpoint.
    if let Some(cs) = &bd.compact_summary {
        rows.push(vec![Span::styled(format!("folded: {}", cs), dim)]);
    }
    rows
}

/// The Memory files drill-down rows: a bold header plus dim suffix, then one
/// tree-glyph row per file (path + tokens). Empty when there are no files.
fn memory_rows(files: &[crate::records::ContextFileEntry]) -> Vec<Vec<Span<'static>>> {
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
    for file in files {
        rows.push(vec![
            Span::styled("\u{2514} ", dim),
            Span::styled(format!("{}: ", file.path), dim),
            Span::styled(format!("{} tokens", fmt_tokens(file.tokens)), dim),
        ]);
    }
    rows
}

/// The Skills drill-down rows: a bold header plus dim suffix, then per-source
/// groups (dim source label + tree-glyph rows). Empty when there are no skills.
fn skills_rows(skills: &[crate::records::ContextSkillEntry]) -> Vec<Vec<Span<'static>>> {
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
    // Group by source preserving first-seen order.
    let mut sources: Vec<String> = Vec::new();
    for s in skills {
        if !sources.iter().any(|x| x == &s.source) {
            sources.push(s.source.clone());
        }
    }
    for src in &sources {
        rows.push(vec![Span::styled(src.clone(), dim)]);
        let group: Vec<&crate::records::ContextSkillEntry> =
            skills.iter().filter(|s| &s.source == src).collect();
        let n = group.len();
        for (i, s) in group.iter().enumerate() {
            let glyph = if i + 1 == n { "\u{2514}" } else { "\u{251C}" };
            rows.push(vec![
                Span::styled(format!("{glyph} "), dim),
                Span::styled(format!("{}: ", s.name), dim),
                Span::styled(format!("{} tokens", fmt_tokens(s.tokens)), dim),
            ]);
        }
    }
    rows
}

/// The Suggestions rows: a bold header, then per suggestion a title row
/// (warning/info glyph + bold title + optional dim savings) and an indented
/// dim detail row. Empty when there are no suggestions.
fn suggestions_rows(suggestions: &[crate::records::ContextSuggestion]) -> Vec<Vec<Span<'static>>> {
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
                format!(" \u{2192} save ~{}", fmt_tokens(savings)),
                dim,
            ));
        }
        rows.push(title);
        rows.push(vec![Span::raw("  "), Span::styled(sug.detail.clone(), dim)]);
    }
    rows
}

/// Compact token figure: 1.2M / 12.3K / 999. Matches the formatter
/// shape so the legend reads like the source analyzer.
fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
#[path = "context_view_tests.rs"]
mod tests;
