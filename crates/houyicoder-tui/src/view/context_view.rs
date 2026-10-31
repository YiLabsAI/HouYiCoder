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
use crate::view::byte_size::format_byte_size;
use crate::view::line_wrap::wrap_hanging;
use crate::view::skill_origin::cost_label;
use crate::view::skill_origin::display_order;
use crate::view::skill_origin::heading_spans;
use crate::view::skill_origin::heading_text;
use houyicoder_protocol::frontend::context::{
    CasAssemblyUsage, CasSizeUnit, CasUsage, ContextBreakdown, DelegatedContext, GridCell,
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
        out.push(Line::from(spans));
    }
    // Drill-down (Memory files, Skills, Delegated) and Suggestions follow,
    // each section preceded by a blank separator row.
    let memory = memory_rows(&view.drill.memory_files);
    let skills = skills_rows(&view.drill.skills);
    let delegated = delegated_rows(bd.delegated.as_ref());
    let storage = cas_rows(bd.cas.as_ref());
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
fn grid_row_spans(bd: &ContextBreakdown, row: &[GridCell], row_start: usize) -> Vec<Span<'static>> {
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
                    "{} ({:.1}%)",
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
        let mut line = format!("Cache prefix: {}", format_tokens(u64::from(prefix)));
        if let Some(rate) = bd.cache_hit_rate {
            line.push_str(&format!(" \u{00b7} hit rate {:.0}%", rate * 100.0));
        }
        rows.push(vec![Span::styled(line, dim)]);
    }
    // Folded summary: present only after a compaction produced a checkpoint.
    if let Some(cs) = &bd.compact_summary {
        rows.push(vec![Span::styled(format!("folded: {}", cs), dim)]);
    }
    // The stored-block account: what the session kept, what it cost raw, and
    // what its newest request carried. A log that records none of it says so
    // rather than printing the zeros of an account that was never kept.
    if let Some(line) = cas_legend(bd.cas.as_ref()) {
        rows.push(vec![Span::styled(line, dim)]);
    }
    rows
}

/// The legend's one block-store line, or None when the session reported no
/// stored-block account at all. A figure the log does not carry reads as
/// unknown; a measured zero stays a zero.
fn cas_legend(cas: Option<&CasUsage>) -> Option<String> {
    let cas = cas?;
    // A build that predates the capture account states no result figure, but
    // its tool results still name the blocks they used, so the marker count is
    // a measured fact and stays on the line.
    if !cas.tracked {
        return Some(if cas.blocks > 0 {
            format!("cas: unknown \u{00b7} {} blocks", cas.blocks)
        } else {
            "cas: unknown".to_string()
        });
    }
    let mut parts = vec![match cas.results {
        Some(results) => format!("{results} results"),
        None => "unknown results".to_string(),
    }];
    parts.push(match cas.raw_bytes {
        Some(bytes) => format!("raw {}", format_byte_size(bytes)),
        None => "raw unknown".to_string(),
    });
    // The assembled size comes from the one formatter the drill uses, so the
    // two representations of one assembly cannot state it differently.
    parts.push(match cas.assembly.as_ref().and_then(assembly_size) {
        Some(size) => format!("model {size}"),
        None => "model unknown".to_string(),
    });
    // A failed save is the fact this account exists to state, so it stays on
    // the line even when the session stored nothing. A count the account
    // measured its results but not its failures for is named as unknown
    // rather than dropped.
    match cas.save_failed {
        Some(failed) if failed > 0 => parts.push(format!("{failed} save failed")),
        None => parts.push("save failures unknown".to_string()),
        Some(_) => {}
    }
    match cas.truncated {
        Some(truncated) if truncated > 0 => parts.push(format!("{truncated} truncated")),
        None => parts.push("truncations unknown".to_string()),
        Some(_) => {}
    }
    Some(format!("cas: {}", parts.join(" \u{00b7} ")))
}

/// An assembly's carried size in the unit its record named, or None when the
/// unit is one this build does not know: a byte count printed as tokens would
/// misstate the cost.
fn assembly_size(assembly: &CasAssemblyUsage) -> Option<String> {
    match assembly.unit {
        CasSizeUnit::Tokens => Some(format!("~{} tokens", format_tokens(assembly.size))),
        CasSizeUnit::Bytes => Some(format_byte_size(assembly.size)),
        CasSizeUnit::Unknown => None,
    }
}

/// The block-store drill-down: the account itself, what the newest request
/// carried, what the host's index covers, and who read a stored result. Empty
/// when the session reported no account. The three stages stay on separate
/// rows, because a request carrying a block, a read of it, and the save that
/// made it readable are different facts.
fn cas_rows(cas: Option<&CasUsage>) -> Vec<Vec<Span<'static>>> {
    let Some(cas) = cas else {
        return Vec::new();
    };
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(vec![
        Span::styled("CAS storage", bold),
        Span::styled(" \u{00b7} /trajectory", dim),
    ]);
    let results = match cas.results {
        Some(n) => n.to_string(),
        None => "unknown".to_string(),
    };
    // A captured figure that some result could not measure is a floor, so the
    // row says how much of the account it does not cover.
    let captured = format!(
        "captured: raw {} \u{00b7} {} save failed \u{00b7} {} truncated",
        cas.raw_bytes
            .map(format_byte_size)
            .unwrap_or_else(|| "unknown".to_string()),
        cas.save_failed
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        cas.truncated
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    );
    let mut lines = vec![
        format!(
            "results: {results} \u{00b7} unique blocks: {} \u{00b7} verified: unknown",
            cas.blocks
        ),
        match cas.unmeasured_results.filter(|n| *n > 0) {
            Some(unmeasured) => format!("{captured} \u{00b7} {unmeasured} unmeasured"),
            None => captured,
        },
        assembly_row(cas.assembly.as_ref()),
        index_row(cas),
        reads_row(cas),
    ];
    if !cas.tracked {
        // The build that wrote this log kept no capture account, so every
        // figure but the marker count is unknown. The rows above still speak
        // for the blocks the tool results name, which the log does measure;
        // this note says why the rest reads as unknown.
        lines.push(
            "this log predates the capture account \u{00b7} only the block count is measured"
                .to_string(),
        );
    }
    let last = lines.len().saturating_sub(1);
    for (i, line) in lines.drain(..).enumerate() {
        rows.push(vec![
            Span::styled(if i == last { "\u{2514} " } else { "\u{251C} " }, dim),
            Span::styled(line, dim),
        ]);
    }
    rows
}

/// Who read a stored result. A log that carries no read record says so
/// instead of reporting a zero the session never measured, and a terminal
/// with no read path leaves its own count unknown.
fn reads_row(cas: &CasUsage) -> String {
    if !cas.reads_recorded {
        return "reads: not recorded".to_string();
    }
    let mut row = format!(
        "reads: model {} \u{00b7} UI {} \u{00b7} failed {} \u{00b7} /trajectory",
        cas.model_reads,
        cas.ui_reads
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        cas.failed_reads,
    );
    if cas.unknown_reads > 0 {
        row.push_str(&format!(" \u{00b7} unknown {}", cas.unknown_reads));
    }
    row
}

/// What the newest assembly carried from the store, in its own unit, with the
/// forms it used. A log with no assembly record leaves the model view unknown
/// rather than reporting a request that carried nothing.
fn assembly_row(assembly: Option<&CasAssemblyUsage>) -> String {
    let Some(assembly) = assembly else {
        return "current assembly: unknown".to_string();
    };
    let mut forms = vec![
        format!("{} full", assembly.full),
        format!("{} preview", assembly.preview),
        format!("{} pointer", assembly.pointer),
    ];
    if assembly.unavailable > 0 {
        forms.push(format!("{} unavailable", assembly.unavailable));
    }
    if assembly.unclassified > 0 {
        forms.push(format!("{} unclassified", assembly.unclassified));
    }
    let size = assembly_size(assembly).unwrap_or_else(|| "unknown size".to_string());
    format!("current assembly: {} ({})", size, forms.join(", "))
}

/// What the host's index covers, as a share of the results the session
/// stored. A host that serves no stored-result request leaves the index
/// unknown rather than reporting an empty one, and a log that cannot count
/// its results leaves the share unknown rather than inventing a denominator.
fn index_row(cas: &CasUsage) -> String {
    let Some(index) = cas.indexed.as_ref() else {
        return "indexed: unavailable".to_string();
    };
    let coverage = if index.complete {
        "complete"
    } else {
        "partial"
    };
    let as_of = index.as_of.as_deref().unwrap_or("unknown");
    let held = match cas.results {
        Some(results) if results > 0 => format!("{}/{}", index.indexed, results),
        Some(_) => index.indexed.to_string(),
        None => format!("{} of unknown", index.indexed),
    };
    let mut row = format!("indexed: {held} \u{00b7} {coverage} \u{00b7} as of event {as_of}");
    if index.corrupt > 0 {
        row.push_str(&format!(" \u{00b7} {} corrupt", index.corrupt));
    }
    row
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
    for file in files {
        rows.push(vec![
            Span::styled("\u{2514} ", dim),
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
        Span::styled("Delegated", bold),
        Span::styled(" \u{00b7} /agents", dim),
    ]);
    rows.push(vec![
        Span::styled("\u{2514} ", dim),
        Span::styled("in context: ", dim),
        Span::styled(
            format!(
                "{} tokens",
                format_tokens(u64::from(delegated.returned_tokens))
            ),
            dim,
        ),
    ]);
    rows.push(vec![
        Span::styled("\u{2514} ", dim),
        Span::styled("returns: ", dim),
        Span::styled(delegated.returns.to_string(), dim),
    ]);
    let child = match delegated.child_tokens {
        Some(tokens) => format!("{} tokens", format_tokens(u64::from(tokens))),
        None => "unknown".to_string(),
    };
    rows.push(vec![
        Span::styled("\u{2514} ", dim),
        Span::styled("children used: ", dim),
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

#[cfg(test)]
mod tests;
