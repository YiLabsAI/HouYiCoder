//! What the screen sanitiser keeps and what it removes.

use super::strip_ansi_codes;

#[test]
fn test_complete_colour_sequence_removed() {
    assert_eq!(
        strip_ansi_codes("\u{1b}[0;32m ok \u{1b}[0m"),
        " ok ",
        "a complete colour sequence leaves no characters behind"
    );
    assert_eq!(
        strip_ansi_codes("\u{1b}[1;38;5;208mwarn\u{1b}[m"),
        "warn",
        "an extended colour and a bare reset both go"
    );
}

#[test]
fn test_orphan_colour_fragment_removed() {
    assert_eq!(strip_ansi_codes("[0;32m ok [0m"), " ok ");
    assert_eq!(strip_ansi_codes("[0m"), "");
    assert_eq!(strip_ansi_codes("[1mready[0m\n"), "ready\n");
}

#[test]
fn test_orphan_matching_stays_narrow() {
    for kept in [
        "arr[0]",
        "#[test]",
        "let v = a[1];",
        "[2026-01-01]",
        "[INFO] started",
        "[a link](https://x)",
    ] {
        assert_eq!(strip_ansi_codes(kept), kept, "must keep: {kept}");
    }
    // A bracketed digit run that ends in the colour terminator is treated as a
    // colour fragment wherever it appears. Prose that spells one out is not
    // distinguishable from the fragment, and tool output carries far more
    // fragments than prose does.
    assert_eq!(strip_ansi_codes("x[10m]"), "x]");
}

#[test]
fn test_osc_title_removed() {
    assert_eq!(
        strip_ansi_codes("\u{1b}]0;a title\u{07}body"),
        "body",
        "an operating system command ends at the bell"
    );
    assert_eq!(
        strip_ansi_codes("\u{1b}]8;;https://x\u{1b}\\link\u{1b}]8;;\u{1b}\\"),
        "link",
        "a hyperlink wrapper ends at the string terminator"
    );
}

#[test]
fn test_plain_text_unchanged() {
    assert_eq!(strip_ansi_codes(""), "");
    assert_eq!(strip_ansi_codes("plain text"), "plain text");
    assert_eq!(
        strip_ansi_codes("中文与 emoji 🎉 保持不变"),
        "中文与 emoji 🎉 保持不变",
        "multi-byte characters survive the scan"
    );
}
