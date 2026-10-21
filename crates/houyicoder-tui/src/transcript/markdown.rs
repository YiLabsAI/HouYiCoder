//! Markdown rendering: parse assistant text into styled terminal lines.
//! Strips raw syntax (##, **, backticks) so copy gets clean text, and
//! applies semantic styling (bold headers, italic, colored inline code,
//! dim code blocks) so the display reads as rendered markdown, not raw
//! source. Markdown is parsed into tokens then each token is mapped
//! to ANSI-styled output; this module does the same with pulldown-cmark
//! events mapped to ratatui spans.

mod normalize;

use crate::view::line_wrap::wrap_hanging;
use crate::view::line_wrap::wrap_line;
use pulldown_cmark::Alignment;
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthStr;

/// The default syntax set (all bundled grammars) and theme set, resolved once
/// and shared for the process lifetime. syntect loads these from its
/// default-assets feature so no external grammar files are needed.
fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    TS.get_or_init(ThemeSet::load_defaults)
}

/// Map a syntect foreground color to a ratatui Rgb color. syntect styles carry
/// an RGBA foreground; ratatui has no alpha, so the alpha is dropped.
fn syn_to_ratatui(s: SynStyle) -> Style {
    let c = s.foreground;
    Style::new().fg(Color::Rgb(c.r, c.g, c.b))
}

/// Build a stateful per-line highlighter for the given language token. Falls
/// back to None (plain text) when the language is unrecognized, so an unknown
/// fence still renders as plain code rather than panicking.
fn make_code_highlighter(lang: &str) -> Option<HighlightLines<'static>> {
    let ss = syntax_set();
    let ts = theme_set();
    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
        .or_else(|| ss.find_syntax_by_name(lang))?;
    let theme = ts.themes.get("base16-ocean.dark")?;
    Some(HighlightLines::new(syntax, theme))
}

/// Style bits accumulated while walking inline content.
#[derive(Clone, Copy, Default)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    code: bool,
}

impl InlineStyle {
    fn to_style(self) -> Style {
        let mut s = Style::new();
        if self.bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.code {
            s = s.fg(Color::Cyan);
        }
        s
    }
}

/// Render markdown text into styled terminal lines plus plain-text rows.
/// The glyph prefix is prepended to the first styled line and the first
/// plain row so the role marker stays consistent with non-markdown rows.
/// Returns (styled_lines for display, plain_rows for copy extraction).
#[expect(unused_assignments, reason = "render loop reassigns")]
#[expect(clippy::too_many_lines, reason = "markdown render")]
#[expect(clippy::cognitive_complexity, reason = "markdown render")]
pub fn render_agent_text(
    glyph: &str,
    text: &str,
    avail: usize,
) -> (Vec<Line<'static>>, Vec<String>) {
    // Enable GFM extensions: tables (without this the Tag::Table arm is dead
    // code — pulldown-cmark parses pipes as a plain paragraph) and task lists.
    // Strikethrough stays off: a single tilde is how a reply writes
    // approximately, and the parser would read a pair of them as a deleted
    // span and drop both.
    let opts = Options::ENABLE_TABLES.union(Options::ENABLE_TASKLISTS);
    let text = normalize::prose(text);
    let parser = Parser::new_ext(text.as_ref(), opts);
    let mut styled: Vec<Line<'static>> = Vec::new();
    let mut plain: Vec<String> = Vec::new();

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain_acc = String::new();
    let mut style = InlineStyle::default();
    let mut first_line = true;
    // pulldown-cmark yields no space tokens between block siblings, so a
    // synthesized separator prevents paragraphs from rendering flush.
    let mut started = false;
    let mut trailing_blank = false;
    let mut in_code_block = false;
    // Stateful per-line syntax highlighter for the current fenced code block.
    // None for indented blocks or unrecognized languages (plain fallback).
    let mut code_highlighter: Option<HighlightLines<'static>> = None;
    let mut in_blockquote = false;
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut table_row: Vec<String> = Vec::new();
    let mut cell_plain = String::new();
    let mut table_aligns: Vec<Alignment> = Vec::new();
    let mut in_heading = false;
    // Stack of (ordered_start, counter) per list depth. None start = bullet
    // list (marker "-"); Some(n) = ordered list starting at n (marker
    // "1.", "2.", ...). The counter ticks per item so ordered lists keep
    // their numbers, matching the canonical list-number rendering.
    let mut list_stack: Vec<(Option<u64>, u64)> = Vec::new();

    /// Flush accumulated spans + plain text into a finished line.
    macro_rules! flush {
        () => {
            if !spans.is_empty() || !plain_acc.is_empty() {
                let plain_prefix = if first_line {
                    format!("{glyph} ")
                } else {
                    " ".repeat(UnicodeWidthStr::width(glyph) + 1)
                };
                if !plain_prefix.is_empty() {
                    let display_prefix = if first_line {
                        format!("{glyph}\u{a0}")
                    } else {
                        plain_prefix.clone()
                    };
                    spans.insert(0, Span::raw(display_prefix));
                    plain_acc = format!("{plain_prefix}{plain_acc}");
                }
                if in_blockquote {
                    let bar = Span::styled("| ", Style::new().fg(Color::DarkGray));
                    spans.insert(0, bar);
                }
                styled.push(Line::from(spans.clone()));
                plain.push(plain_acc.clone());
                spans.clear();
                plain_acc.clear();
                first_line = false;
                started = true;
            }
        };
    }

    /// Emit a blank-line separator before a top-level block when content
    /// already exists and the prior block left no trailing blank.
    macro_rules! block_sep {
        () => {
            if started && list_stack.is_empty() && !trailing_blank {
                styled.push(Line::default());
                plain.push(String::new());
            }
            trailing_blank = false;
        };
    }

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    block_sep!();
                    in_heading = true;
                    style.bold = true;
                    if level == HeadingLevel::H1 {
                        style.italic = true;
                    }
                }
                Tag::Strong => style.bold = true,
                Tag::Emphasis => style.italic = true,
                Tag::CodeBlock(kind) => {
                    block_sep!();
                    in_code_block = true;
                    style.code = true;
                    // Resolve a syntax highlighter from the fence language
                    // (the first token of the info string, e.g. rust). An
                    // unknown language falls back to plain code (the
                    // soft-blue path).
                    let lang = match kind {
                        CodeBlockKind::Fenced(ref s) => s.split(' ').next().unwrap_or("").trim(),
                        _ => "",
                    };
                    code_highlighter = make_code_highlighter(lang);
                }
                Tag::Paragraph => {
                    block_sep!();
                }
                Tag::BlockQuote(_) => {
                    block_sep!();
                    in_blockquote = true;
                    style.italic = true;
                }
                Tag::List(start) => {
                    block_sep!();
                    let init = start.unwrap_or(0);
                    list_stack.push((start, init));
                }
                Tag::Item => {
                    // Flush before each item so a tight list (items with no
                    // blank line between them — the common case for a quick
                    // numbered or bulleted breakdown) renders one item per
                    // row. Without this, tight-list items carry no
                    // Paragraph wrapper (so TagEnd::Paragraph never fires
                    // between them) and every item's text accumulates into a
                    // single wrapped line — the user sees only the first
                    // item, the rest merge, and the tail is clipped by the
                    // terminal width. Loose lists (blank-line-separated)
                    // already flushed via Paragraph; this makes tight lists
                    // match.
                    flush!();
                    let indent = "  ".repeat(list_stack.len().saturating_sub(1));
                    let marker = match list_stack.last_mut() {
                        Some((Some(_), counter)) => {
                            // Ordered list: render "N." and tick the counter
                            // so items keep their numbers, matching the
                            // canonical list-number rendering (bullet
                            // lists render "-"). Use-then-increment: the
                            // list's start value is the first item's number,
                            // so the first item renders "start." not
                            // "start+1." (the prior increment-first path was
                            // off by one — every list began at "2.").
                            let n = *counter;
                            *counter += 1;
                            format!("{indent}{}. ", n)
                        }
                        _ => format!("{indent}- "),
                    };
                    spans.push(Span::raw(marker.clone()));
                    plain_acc.push_str(&marker);
                }
                Tag::Table(v) => {
                    block_sep!();
                    flush!();
                    in_table = true;
                    table_aligns = v.clone();
                    table_rows.clear();
                }
                Tag::TableHead | Tag::TableRow => {
                    table_row.clear();
                }
                Tag::TableCell => {
                    cell_plain.clear();
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    in_heading = false;
                    style.bold = false;
                    style.italic = false;
                    flush!();
                    // A trailing blank line follows every
                    // heading; pulldown-cmark does
                    // not, so synthesize it and mark that the next block
                    // must not add another (would double the blank).
                    styled.push(Line::default());
                    plain.push(String::new());
                    trailing_blank = true;
                }
                TagEnd::Strong => style.bold = false,
                TagEnd::Emphasis => style.italic = false,
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    style.code = false;
                    code_highlighter = None;
                    flush!();
                }
                TagEnd::Paragraph => {
                    flush!();
                }
                TagEnd::BlockQuote(_) => {
                    in_blockquote = false;
                    style.italic = false;
                    flush!();
                }
                TagEnd::List(_) => {
                    flush!(); // last item not otherwise flushed; would weld into next block
                    list_stack.pop();
                }
                TagEnd::TableCell => {
                    table_row.push(cell_plain.clone());
                    cell_plain.clear();
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    table_rows.push(table_row.clone());
                    table_row.clear();
                }
                TagEnd::Table => {
                    flush!();
                    in_table = false;
                    let rows = render_table(
                        &table_rows,
                        &table_aligns,
                        avail.saturating_sub(UnicodeWidthStr::width(glyph) + 1),
                    );
                    table_rows.clear();
                    for line in rows {
                        let display = if first_line {
                            first_line = false;
                            format!("{glyph} {line}")
                        } else {
                            format!("{}{line}", " ".repeat(UnicodeWidthStr::width(glyph) + 1))
                        };
                        styled.push(Line::from(Span::raw(display.clone())));
                        plain.push(display);
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                // A fenced block is copied verbatim by the prose pass, so an
                // anchor-shaped character inside it is the author's own.
                let t = if in_code_block {
                    Cow::Borrowed(t.as_ref())
                } else {
                    normalize::without_anchor(&t)
                };
                let t = if in_heading { t.trim() } else { t.as_ref() };
                if in_table {
                    cell_plain.push_str(t);
                } else if in_code_block {
                    let plain_fallback =
                        |line: &str, spans: &mut Vec<Span<'static>>, acc: &mut String| {
                            spans.push(Span::styled(
                                line.to_string(),
                                Style::new().fg(Color::Rgb(160, 185, 225)),
                            ));
                            acc.push_str(line);
                        };
                    for (i, line) in t.split('\n').enumerate() {
                        if i > 0 {
                            flush!();
                        }
                        if let Some(h) = code_highlighter.as_mut() {
                            match h.highlight_line(line, syntax_set()) {
                                Ok(ranges) => {
                                    if ranges.is_empty() {
                                        plain_fallback(line, &mut spans, &mut plain_acc);
                                    } else {
                                        for (st, txt) in ranges {
                                            spans.push(Span::styled(
                                                txt.to_string(),
                                                syn_to_ratatui(st),
                                            ));
                                            plain_acc.push_str(txt);
                                        }
                                    }
                                }
                                Err(_) => plain_fallback(line, &mut spans, &mut plain_acc),
                            }
                        } else {
                            plain_fallback(line, &mut spans, &mut plain_acc);
                        }
                    }
                } else {
                    spans.push(Span::styled(t.to_string(), style.to_style()));
                    plain_acc.push_str(t);
                }
            }
            Event::Code(t) => {
                if in_table {
                    cell_plain.push_str(&t);
                } else {
                    spans.push(Span::styled(t.to_string(), Style::new().fg(Color::Cyan)));
                    plain_acc.push_str(&t);
                }
            }
            Event::SoftBreak => {
                // A soft break (a single newline in the source within a
                // paragraph) starts a new rendered line — CommonMark renders
                // soft breaks as newlines, and a coding agent's reply uses
                // single newlines to separate lines the user expects to
                // select individually. Flushing here splits a multi-line
                // reply into one row per line so each line is independently
                // selectable + copyable. The glyph prefix only applies to the
                // first line (first_line guard in the flush macro).
                flush!();
            }
            Event::HardBreak => {
                flush!();
            }
            Event::DisplayMath(_) | Event::InlineMath(_) => {}
            Event::FootnoteReference(_) | Event::TaskListMarker(_) => {}
            _ => {}
        }
    }
    flush!();
    wrap_markdown(styled, plain, avail, glyph)
}

/// Soft-wrap markdown lines. Continuation rows hang at glyph width + 1.
/// The non-breaking display prefix prevents splitting the glyph onto its
/// own row. avail 0 leaves lines whole.
fn wrap_markdown(
    styled: Vec<Line<'static>>,
    plain: Vec<String>,
    avail: usize,
    glyph: &str,
) -> (Vec<Line<'static>>, Vec<String>) {
    if avail == 0 || styled.len() != plain.len() {
        return (styled, plain);
    }
    let indent_w = UnicodeWidthStr::width(glyph) + 1;
    let mut out_s: Vec<Line<'static>> = Vec::with_capacity(styled.len());
    let mut out_p: Vec<String> = Vec::with_capacity(styled.len());
    for (line, _orig_plain) in styled.into_iter().zip(plain) {
        // One budget for the first row and its continuations: the row already
        // carries the glyph indent, so wrapping the full width and re-adding
        // the indent afterwards would make every continuation wider than the
        // row above it.
        for row in wrap_hanging(line, indent_w, avail) {
            let p: String = row
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .replace('\u{a0}', " ");
            out_s.push(row);
            out_p.push(p);
        }
    }
    (out_s, out_p)
}

/// Render a markdown table as ASCII: header row, separator, data rows.
///
/// Column widths are the widest cell per column, then shrunk to the width the
/// pane can spare. A cell wider than its column is wrapped inside the cell and
/// the row grows downward, so the pipe separators stay in one column on every
/// visual row. Wrapping the finished table as prose would break the header's
/// trailing pipe and every column after it.
fn render_table(rows: &[Vec<String>], aligns: &[Alignment], avail: usize) -> Vec<String> {
    if rows.is_empty() || rows[0].is_empty() {
        return Vec::new();
    }
    let ncols = rows[0].len();
    let mut widths: Vec<usize> = (0..ncols)
        .map(|c| {
            let mut w = 3usize;
            for row in rows {
                if let Some(cell) = row.get(c) {
                    w = w.max(UnicodeWidthStr::width(cell.as_str()));
                }
            }
            w
        })
        .collect();
    const MIN_COL: usize = 4;
    let spent = |widths: &[usize]| widths.iter().sum::<usize>() + 3 * ncols + 1;
    if avail > 0 {
        while spent(&widths) > avail {
            let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
                break;
            };
            if widths[widest] <= MIN_COL {
                break;
            }
            widths[widest] -= 1;
        }
        if spent(&widths) > avail {
            return stacked_table(rows, avail);
        }
    }
    let align_of = |c: usize| aligns.get(c).copied().unwrap_or(Alignment::None);
    let mut out = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let cells: Vec<Vec<String>> = (0..ncols)
            .map(|c| {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                wrap_line(cell, widths[c])
            })
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for line_idx in 0..height {
            let mut line = String::from("| ");
            for (c, cell_rows) in cells.iter().enumerate() {
                let text = cell_rows.get(line_idx).map(String::as_str).unwrap_or("");
                line.push_str(&pad_cell(text, widths[c], align_of(c)));
                line.push_str(" | ");
            }
            out.push(line.trim_end().to_string());
        }
        if r == 0 {
            let mut sep = String::from('|');
            for w in &widths {
                sep.push_str(&"-".repeat(w + 2));
                sep.push('|');
            }
            out.push(sep);
        }
    }
    out
}

/// The table a pane too narrow for any column layout falls back to: one
/// header-and-value pair per line, wrapped to the pane. A table that cannot
/// keep its columns aligned must stop pretending to be a table.
fn stacked_table(rows: &[Vec<String>], avail: usize) -> Vec<String> {
    let headers = &rows[0];
    if rows.len() == 1 {
        // A header with no body is still content: state its columns instead of
        // dropping the table.
        return wrap_line(&headers.join(" | "), avail);
    }
    let mut out = Vec::new();
    for (r, row) in rows.iter().enumerate().skip(1) {
        if r > 1 {
            out.push(String::new());
        }
        for (c, cell) in row.iter().enumerate() {
            let label = headers.get(c).map(String::as_str).unwrap_or("");
            let prefix = format!("{label}: ");
            let indent = UnicodeWidthStr::width(prefix.as_str());
            // A label wider than the pane leaves no hanging column: wrap the
            // pair as one line instead of printing a prefix that already runs
            // past the edge.
            if indent + 4 > avail {
                out.extend(wrap_line(&format!("{prefix}{cell}"), avail));
                continue;
            }
            let budget = avail - indent;
            for (i, line) in wrap_line(cell, budget).iter().enumerate() {
                if i == 0 {
                    out.push(format!("{prefix}{line}"));
                } else {
                    out.push(format!("{}{}", " ".repeat(indent), line));
                }
            }
        }
    }
    out
}

/// Pad a cell to the column width per its alignment. Left = trailing spaces,
/// right = leading spaces, center = split. None and Left both render
/// left-aligned. The pad-aligned rule: dash-only separator, no
/// alignment colons rendered.
fn pad_cell(content: &str, width: usize, align: Alignment) -> String {
    let display_w = UnicodeWidthStr::width(content);
    let pad = width.saturating_sub(display_w);
    match align {
        Alignment::Center => {
            let left = pad / 2;
            format!("{}{}{}", " ".repeat(left), content, " ".repeat(pad - left))
        }
        Alignment::Right => format!("{}{}", " ".repeat(pad), content),
        Alignment::Left | Alignment::None => {
            format!("{content}{}", " ".repeat(pad))
        }
    }
}

#[test]
fn test_ordered_list_keeps_numbers() {
    // A numbered breakdown where each item carries a paragraph + a nested
    // bullet sub-list (a common agent-reply shape). Each "N." must start
    // its own rendered line and keep its source number — a prior
    // increment-first counter made every list begin at "2.".
    let md = "1. Host-Guest model: host holds all trust\n\n   - control plane != model plane\n2. Token economy\n\n   - TokenBudgetPlanner\n3. Reproducibility\n4. Security\n";
    let (_, plain) = render_agent_text("●", md, 80);
    let joined = plain.join("\n");
    assert!(joined.contains("1. Host-Guest"), "item 1 marker: {joined}");
    assert!(
        joined.contains("2. Token economy"),
        "item 2 marker: {joined}"
    );
    assert!(
        joined.contains("3. Reproducibility"),
        "item 3 marker: {joined}"
    );
    assert!(joined.contains("4. Security"), "item 4 marker: {joined}");
    // No off-by-one bleed: "5." must never appear for a four-item list.
    assert!(
        !joined.contains("\n5. "),
        "no off-by-one fifth marker: {joined}"
    );
}

#[test]
fn test_ordered_list_no_merge() {
    // Tight ordered list (no blank lines between items): each item still
    // gets its own line + number; they must not merge into one paragraph.
    let md = "1. first item\n2. second item\n3. third item\n";
    let (_, plain) = render_agent_text("●", md, 80);
    // Three non-empty item lines, each carrying its marker.
    let item_lines: Vec<&String> = plain.iter().filter(|s| !s.is_empty()).collect();
    assert!(
        item_lines.iter().any(|s| s.contains("1. first")),
        "item 1: {plain:?}"
    );
    assert!(
        item_lines.iter().any(|s| s.contains("2. second")),
        "item 2: {plain:?}"
    );
    assert!(
        item_lines.iter().any(|s| s.contains("3. third")),
        "item 3: {plain:?}"
    );
}

/// A long agent paragraph soft-wraps: render_agent_text returns >1 styled
/// line, styled + plain stay length-locked (the render zip never truncates),
/// and a wide width does not wrap. Pins the markdown leg of count==render at
/// a wrap-forcing width (diff + stdout legs have their own).
#[test]
fn test_wraps_long_paragraph_narrow() {
    let long = "this is a very long agent paragraph that must soft-wrap to multiple styled rows at a narrow pane width and not drift the count";
    let (styled, plain) = render_agent_text("●", long, 20);
    assert!(
        styled.len() > 1,
        "must wrap to >1 row at width 20, got {}",
        styled.len()
    );
    assert_eq!(
        styled.len(),
        plain.len(),
        "styled+plain lockstep after wrap"
    );
    assert_eq!(render_agent_text("●", long, 20).0.len(), styled.len());
    let (wide, _) = render_agent_text("●", long, 200);
    assert_eq!(wide.len(), 1, "no wrap at width 200");
}

#[test]
fn test_list_item_code_kept() {
    // A list item whose text spans multiple source lines with an inline
    // code span on the second line must not glue the trailing word of
    // line 1 onto the code span of line 2 ("CI" + "check_dep_graph.py"
    // -> "CIcheck_dep_graph.py"). Soft breaks start new rendered lines.
    let md = "1. enforced by CI\n`check_dep_graph.py` machine check\n";
    let (_, plain) = render_agent_text("●", md, 80);
    let joined = plain.join("\n");
    assert!(
        !joined.contains("CIcheck_dep_graph"),
        "inline code glued to prior line's tail: {joined}"
    );
    assert!(
        joined.contains("check_dep_graph"),
        "code span kept: {joined}"
    );
}

#[cfg(test)]
mod tests;
