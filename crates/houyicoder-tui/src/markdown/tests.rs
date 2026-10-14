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
    // ENABLE_TABLES on: table parsed + rendered with padded cells.
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
    // A display-width-2 glyph must count as 2 for column width.
    let md = "| z | 🌐 |\n|---|---|\n| 1 | 2 |";
    let (_, plain) = render_agent_text("●", md, 80);
    let joined = plain.join("\n");
    assert!(joined.contains("🌐"), "wide-glyph cell rendered: {joined}");
}

#[test]
fn test_table_right_align() {
    // Right-aligned column: padding before content so the right edge aligns.
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
    assert_eq!(plain[1], "  - second", "row 1: {}", plain[1]);
    assert_eq!(plain[2], "  - third", "row 2: {}", plain[2]);
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
    assert_eq!(plain[2], "  second para");
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
    assert_eq!(plain[2], "  body");
    assert!(plain.len() <= 3, "no doubled blank: {:?}", plain);
}
