//! The prose rewrites, read on their own so a failure names the transform
//! rather than the rendered line.

use super::*;

/// The text with every anchor written out, so an assertion reads the exact
/// bytes the parser receives.
fn anchored(text: &str) -> String {
    prose(text).replace(ANCHOR, "<a>")
}

#[test]
fn test_bold_after_chinese() {
    // The opening run touches Chinese text on the left and a quote on the
    // right, which the flanking rule rejects; the anchor is what makes it a
    // valid opening run.
    assert_eq!(
        anchored("是**\"模型切换\"**（x）"),
        "是**<a>\"模型切换\"<a>**（x）"
    );
}

#[test]
fn test_runs_pair_in_order() {
    // Without the anchors the parser pairs the second run with the third and
    // bolds the text between them. Anchoring each pair keeps the order.
    assert_eq!(
        anchored("是**\"模型切换\"**，以及**\"缓存命中\"**"),
        "是**<a>\"模型切换\"<a>**，以及**<a>\"缓存命中\"<a>**"
    );
}

#[test]
fn test_trailing_space_before_close() {
    // A space between the closing run and the sentence end is how a reply
    // often ends a bold span; the run still pairs and still closes.
    assert_eq!(
        anchored("读得准**不等于**归因准。 **"),
        "读得准**<a>不等于<a>**归因准。 **"
    );
}

#[test]
fn test_code_span_untouched() {
    let text = "use `是**\"x\"**` here";
    assert_eq!(prose(text), text);
}

#[test]
fn test_fenced_block_untouched() {
    let text = "```\n是**\"x\"** ①\n```\n";
    assert_eq!(prose(text), text);
}

#[test]
fn test_unmatched_run_kept() {
    assert_eq!(prose("a ** b"), "a ** b");
    assert_eq!(prose("**a"), "**a");
}

#[test]
fn test_blank_line_breaks_pairing() {
    assert_eq!(prose("**a\n\nb**"), "**a\n\nb**");
}

#[test]
fn test_table_cells_pair_alone() {
    // A cell boundary is a block boundary: two runs in one cell pair with
    // each other, not with a run in the neighbouring cell.
    assert_eq!(
        anchored("| **a** | **b** |"),
        "| **<a>a<a>** | **<a>b<a>** |"
    );
}

#[test]
fn test_escaped_run_kept() {
    assert_eq!(prose(r"a \*\* b"), r"a \*\* b");
}

#[test]
fn test_digits_mapped() {
    assert_eq!(prose("⓪①②⑳㉑㊿❶⓵"), "(0)(1)(2)(20)(21)(50)(1)(1)");
}

#[test]
fn test_digits_in_code_kept() {
    assert_eq!(prose("`①`"), "`①`");
    assert_eq!(prose("```\n①\n```\n"), "```\n①\n```\n");
}

#[test]
fn test_plain_text_borrowed() {
    assert!(matches!(prose("no syntax here"), Cow::Borrowed(_)));
}

#[test]
fn test_anchor_removed() {
    assert_eq!(without_anchor("a\u{200b}b"), "ab");
    assert!(matches!(without_anchor("ab"), Cow::Borrowed(_)));
}

/// A run whose span would open on whitespace is not a bold opener in any
/// reading, so a pair written that way stays literal.
#[test]
fn test_pair_with_space_kept() {
    assert_eq!(prose("x ** y ** z"), "x ** y ** z");
    assert_eq!(prose("**\nx**"), "**\nx**");
}

/// A code span may open on one line and close on a later one, and its content
/// is copied as it stands.
#[test]
fn test_code_span_across_lines() {
    let text = "`a\n**x** ①`";
    assert_eq!(prose(text), text);
}

/// A backtick with no closing run anywhere is literal text, so the prose
/// around it is still rewritten.
#[test]
fn test_lone_backtick_is_prose() {
    assert_eq!(prose("a ` b ①"), "a ` b (1)");
}

/// A code span cannot cross a blank line, so a backtick whose only partner
/// sits in a later paragraph is literal text and the prose is still rewritten.
#[test]
fn test_span_stops_at_blank() {
    assert_eq!(prose("`a\n\n①`"), "`a\n\n(1)`");
}

/// A fence interrupts the paragraph, so a run inside it does not close a span
/// that opened before it.
#[test]
fn test_span_stops_at_fence() {
    let text = "`a\n```\nx`\n```\n";
    assert_eq!(prose(text), text);
}
