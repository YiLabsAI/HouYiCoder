//! Markdown rendering: parse assistant text into styled terminal lines.
//! Strips raw syntax (##, **, backticks) so copy gets clean text, and
//! applies semantic styling (bold headers, italic, colored inline code,
//! dim code blocks) so the display reads as rendered markdown, not raw
//! source. Markdown is parsed into tokens then each token is mapped
//! to ANSI-styled output; this module does the same with pulldown-cmark
//! events mapped to ratatui spans.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
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
    // code — pulldown-cmark parses pipes as a plain paragraph), strikethrough,
    // task lists. GFM-style markdown handling.
    let opts = Options::ENABLE_TABLES
        .union(Options::ENABLE_STRIKETHROUGH)
        .union(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(text, opts);
    let mut styled: Vec<Line<'static>> = Vec::new();
    let mut plain: Vec<String> = Vec::new();

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain_acc = String::new();
    let mut style = InlineStyle::default();
    let mut first_line = true;
    // Have we emitted any block content yet? pulldown-cmark, unlike a
    // whitespace-tokenizing parser, yields no space tokens between block siblings, so
    // without a synthesized separator every paragraph/list/code block
    // would render flush against the next (the cramped look). The
    // inter-block blank line comes from a synthesized separator
    // plus each heading's own trailing double-newline; we synthesize the
    // equivalent here.
    let mut started = false;
    // True when the previous block already emitted a trailing blank line
    // (a heading does, so the next block must not add another — that
    // would produce two blank lines, which the renderer avoids).
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
    let mut table_aligns: Vec<pulldown_cmark::Alignment> = Vec::new();
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
                let prefix = if first_line {
                    format!("{glyph} ")
                } else {
                    String::new()
                };
                if !prefix.is_empty() {
                    let pfx = prefix.clone();
                    spans.insert(0, Span::raw(pfx));
                    plain_acc = format!("{prefix}{plain_acc}");
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
    /// already exists and the prior block did not already leave a trailing
    /// blank (a heading does). Stands in for a parser space
    /// token between block siblings; skipped inside lists where tight/loose
    /// spacing is governed by the item flush.
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
                        pulldown_cmark::CodeBlockKind::Fenced(ref s) => {
                            s.split(' ').next().unwrap_or("").trim()
                        }
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
                    let rows = render_table(&table_rows, &table_aligns);
                    table_rows.clear();
                    for line in rows {
                        let display = if first_line {
                            first_line = false;
                            format!("{glyph} {line}")
                        } else {
                            line
                        };
                        styled.push(Line::from(Span::raw(display.clone())));
                        plain.push(display);
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                let t = if in_heading { t.trim() } else { &t };
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
    wrap_markdown(styled, plain, avail)
}

/// Soft-wrap each emitted markdown line to the available pane width, mirroring
/// the count path (line_display_rows) to the render path so a long agent line
/// wraps on both — no count/render drift. The plain vec stays parallel: each
/// wrapped row's plain is the joined text of its styled spans. avail 0
/// (unknown width, e.g. before the first render) leaves lines whole so the
/// caller falls back to the terminal's truncation, never panics.
fn wrap_markdown(
    styled: Vec<Line<'static>>,
    plain: Vec<String>,
    avail: usize,
) -> (Vec<Line<'static>>, Vec<String>) {
    if avail == 0 || styled.len() != plain.len() {
        return (styled, plain);
    }
    let mut out_s: Vec<Line<'static>> = Vec::with_capacity(styled.len());
    let mut out_p: Vec<String> = Vec::with_capacity(styled.len());
    for (line, _orig_plain) in styled.into_iter().zip(plain) {
        let rows = crate::view::line_wrap::wrap_styled_line(line, avail);
        for row in rows {
            // The plain for a wrapped row is the joined span content.
            let p: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            out_s.push(row);
            out_p.push(p);
        }
    }
    (out_s, out_p)
}

/// Render a markdown table as ASCII: header row, separator, data rows.
/// Column widths are max display width per column (CJK-aware, min 3). Cells
/// are padded per the column alignment so the pipe separators line up.
/// The table algorithm: column-width min 3, dash-only
/// separator with no alignment colons, CJK-aware display width.
fn render_table(rows: &[Vec<String>], aligns: &[pulldown_cmark::Alignment]) -> Vec<String> {
    if rows.is_empty() || rows[0].is_empty() {
        return Vec::new();
    }
    let ncols = rows[0].len();
    let widths: Vec<usize> = (0..ncols)
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
    let align_of = |c: usize| {
        aligns
            .get(c)
            .copied()
            .unwrap_or(pulldown_cmark::Alignment::None)
    };
    let mut out = Vec::new();
    let mut header = String::from("| ");
    for (c, cell) in rows[0].iter().enumerate() {
        header.push_str(&pad_cell(cell, widths[c], align_of(c)));
        header.push_str(" | ");
    }
    out.push(header.trim_end().to_string());
    let mut sep = String::from('|');
    for w in &widths {
        sep.push_str(&"-".repeat(w + 2));
        sep.push('|');
    }
    out.push(sep);
    for row in rows.iter().skip(1) {
        let mut line = String::from("| ");
        for (c, cell) in row.iter().enumerate() {
            line.push_str(&pad_cell(
                cell,
                widths.get(c).copied().unwrap_or(3),
                align_of(c),
            ));
            line.push_str(" | ");
        }
        out.push(line.trim_end().to_string());
    }
    out
}

/// Pad a cell to the column width per its alignment. Left = trailing spaces,
/// right = leading spaces, center = split. None and Left both render
/// left-aligned. The pad-aligned rule: dash-only separator, no
/// alignment colons rendered.
fn pad_cell(content: &str, width: usize, align: pulldown_cmark::Alignment) -> String {
    let display_w = UnicodeWidthStr::width(content);
    let pad = width.saturating_sub(display_w);
    match align {
        pulldown_cmark::Alignment::Center => {
            let left = pad / 2;
            format!("{}{}{}", " ".repeat(left), content, " ".repeat(pad - left))
        }
        pulldown_cmark::Alignment::Right => format!("{}{}", " ".repeat(pad), content),
        pulldown_cmark::Alignment::Left | pulldown_cmark::Alignment::None => {
            format!("{content}{}", " ".repeat(pad))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strips_heading_syntax() {
        // Heading emits a trailing blank line (the canonical
        // trailing \n\n), so the body lands two rows
        // after the heading, not one.
        let (lines, plain) = render_agent_text("●", "## Title\nbody", 80);
        assert_eq!(plain[0], "● Title");
        assert!(!plain[0].contains("##"));
        assert_eq!(plain[1], "", "row 1 is the heading trailing blank");
        assert!(!plain[2].is_empty(), "row 2 is the body");
        assert_eq!(lines.len(), plain.len());
    }

    #[test]
    fn test_strips_bold_italic() {
        let (_, plain) = render_agent_text("●", "This is **bold** and *italic*.", 80);
        let joined = plain.join("\n");
        assert!(!joined.contains("**"));
        assert!(!joined.contains('*'));
        assert!(joined.contains("bold"));
        assert!(joined.contains("italic"));
    }

    #[test]
    fn test_strips_inline_code() {
        let (_, plain) = render_agent_text("●", "Use `cargo build` to compile.", 80);
        let joined = plain.join("\n");
        assert!(!joined.contains('`'));
        assert!(joined.contains("cargo build"));
    }

    #[test]
    fn test_code_block_plain() {
        let md = "```rust\nfn main() {}\n```";
        let (_, plain) = render_agent_text("●", md, 80);
        let joined = plain.join("\n");
        assert!(!joined.contains("```"));
        assert!(joined.contains("fn main()"));
    }

    #[test]
    fn test_glyph_on_first_line() {
        let (lines, plain) = render_agent_text("●", "para one\n\npara two", 80);
        assert!(plain[0].starts_with("● "));
        assert!(!plain[1].starts_with("● "));
        assert_eq!(lines.len(), plain.len());
    }

    #[test]
    fn test_list_items_plain() {
        let (_, plain) = render_agent_text("●", "- first\n- second", 80);
        let joined = plain.join("\n");
        assert!(joined.contains("- first"));
        assert!(joined.contains("- second"));
    }

    #[test]
    fn test_empty_text_no_lines() {
        let (lines, plain) = render_agent_text("●", "", 80);
        assert!(lines.is_empty());
        assert!(plain.is_empty());
    }

    #[test]
    fn test_renders_table_pipe_format() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |";
        let (_, plain) = render_agent_text("●", md, 80);
        let joined = plain.join("\n");
        // ENABLE_TABLES on: render_table fires, padding cells to the column
        // width (min 3) + a width+2 dash separator. This asserts the table
        // was parsed + rendered (not raw pipes as a paragraph).
        assert!(joined.contains("| a   | b   |"), "header padded: {joined}");
        assert!(
            joined.contains("|-----|-----|"),
            "sep width+2 dashes: {joined}"
        );
        assert!(joined.contains("| 1   | 2   |"), "data padded: {joined}");
    }

    #[test]
    fn test_table_column_width_aligned() {
        let md = "| x | yy |\n|---|---|\n| abbb | c |";
        let (_, plain) = render_agent_text("●", md, 80);
        let joined = plain.join("\n");
        assert!(joined.contains("abbb"), "wider cell: {joined}");
    }

    #[test]
    fn test_table_wide_glyph_width() {
        // a display-width-2 glyph in a cell must count as 2 for column
        // width + padding, not as 1 char, so the pipe separators still
        // align. Guards the UnicodeWidthStr call in render_table.
        let md = "| z | 🌐 |\n|---|---|\n| 1 | 2 |";
        let (_, plain) = render_agent_text("●", md, 80);
        let joined = plain.join("\n");
        assert!(joined.contains("🌐"), "wide-glyph cell rendered: {joined}");
    }

    #[test]
    fn test_table_right_align() {
        // right-aligned column: padding goes BEFORE the content so the
        // right edge aligns. The header separator dashes-colon sets right.
        let md = "| name | val |\n|:---|---:|\n| x | 1 |\n| abc | 22 |";
        let (_, plain) = render_agent_text("●", md, 80);
        let joined = plain.join("\n");
        // val column width = max(val,1,22) = 3. right-aligned cell = pad
        // then content; line is pipe-space + cell + space-pipe, so a width-3
        // right cell renders as 2 leading pad + 1 char = 3 between the
        // pipe-spaces. Single-digit "1" -> "|   1 |", two-digit "22" ->
        // "|  22 |".
        assert!(
            joined.contains("|   1 |"),
            "right-aligned single-digit cell pads left: {joined}"
        );
        assert!(
            joined.contains("|  22 |"),
            "right-aligned two-digit cell pads left: {joined}"
        );
    }

    #[test]
    fn test_table_center_align_splits() {
        // center-aligned: padding split left/right. The colon-dashes-colon
        // separator sets center alignment.
        let md = "| a | b |\n|:--:|:--:|\n| 1 | 22 |";
        let (_, plain) = render_agent_text("●", md, 80);
        let joined = plain.join("\n");
        // col width = 3 (min). "1" center: pad=2, left=1 -> " 1 " between
        // pipe-spaces -> "|  1  |". "22" center: pad=1, left=0 -> "22 " ->
        // "| 22  |" (trailing pad lands before the closing space-pipe).
        assert!(
            joined.contains("|  1  |"),
            "center align on single-char cell: {joined}"
        );
    }

    #[test]
    fn test_tight_list_rows() {
        // A tight list (single newlines, no blank lines between items) must
        // render one item per row. Without the Item-start flush, tight-list
        // items carry no Paragraph wrapper so they merged into one wrapped
        // line — the user saw only the first item + a clipped tail.
        let md = "- first\n- second\n- third";
        let (_, plain) = render_agent_text("●", md, 80);
        assert_eq!(
            plain.len(),
            3,
            "one row per item, got {}: {:?}",
            plain.len(),
            plain
        );
        assert!(plain[0].ends_with("- first"), "row 0: {}", plain[0]);
        assert_eq!(plain[1], "- second", "row 1: {}", plain[1]);
        assert_eq!(plain[2], "- third", "row 2: {}", plain[2]);
    }

    #[test]
    fn test_tight_numbered_list_rows() {
        // Numbered tight list: each item its own row (the merge bug would
        // join all into one).
        let md = "1. alpha\n2. beta\n3. gamma";
        let (_, plain) = render_agent_text("●", md, 80);
        assert_eq!(plain.len(), 3, "got {}: {:?}", plain.len(), plain);
        assert!(plain[0].contains("alpha"));
        assert!(plain[1].contains("beta"));
        assert!(plain[2].contains("gamma"));
    }

    #[test]
    fn test_list_after_paragraph() {
        // A paragraph followed by a tight list: the list items are separate
        // rows, not merged into the paragraph.
        let md = "intro line\n- a\n- b";
        let (_, plain) = render_agent_text("●", md, 80);
        assert!(plain.len() >= 3, "got {}: {:?}", plain.len(), plain);
    }

    #[test]
    fn test_block_separator_between_paragraphs() {
        // Two paragraphs separated by a blank line in the source must keep
        // a blank line between them in the output (synthesized
        // since pulldown-cmark yields no space token; without it the
        // two paragraphs would render flush). Without the separator
        // the two paragraphs render flush (the cramped look).
        let md = "first para\n\nsecond para";
        let (_, plain) = render_agent_text("●", md, 80);
        assert_eq!(plain.len(), 3, "got {}: {:?}", plain.len(), plain);
        assert_eq!(plain[0], "\u{25cf} first para");
        assert_eq!(plain[1], "", "blank separator between paragraphs");
        assert_eq!(plain[2], "second para");
    }

    #[test]
    fn test_heading_paragraph_blank() {
        // A heading emits one trailing blank (its own), so the next block
        // must NOT add another (would double the blank, which the
        // renderer must not produce).
        let md = "## Title\n\nbody";
        let (_, plain) = render_agent_text("●", md, 80);
        assert_eq!(plain[0], "\u{25cf} Title");
        assert_eq!(plain[1], "", "single trailing blank after heading");
        assert_eq!(plain[2], "body");
        assert!(plain.len() <= 3, "no doubled blank: {:?}", plain);
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
