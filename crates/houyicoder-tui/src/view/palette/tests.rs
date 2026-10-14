use super::*;
use unicode_width::UnicodeWidthStr;

#[test]
fn test_pad_name_aligns() {
    // short names pad to NAME_COL so help columns align
    assert_eq!(pad_name("/a"), format!("/a{}", " ".repeat(NAME_COL - 2)));
    // names still under NAME_COL also pad to NAME_COL
    assert_eq!(pad_name("/release-notes"), format!("/release-notes  "));
}

#[test]
fn test_area_bottom_anchored() {
    let screen = Rect::new(0, 0, 80, 24);
    let a = area(screen);
    assert!(a.height <= 12, "popover must stay small, got {}", a.height);
    assert!(
        a.y + a.height <= screen.y + screen.height,
        "popover must not overflow the screen"
    );
    assert!(
        a.width <= MAX_WIDTH,
        "popover width must be capped, got {}",
        a.width
    );
    // centered: equal margin both sides
    assert_eq!(
        a.x - screen.x,
        (screen.width - a.width) / 2,
        "popover should be horizontally centered"
    );
}

#[test]
fn test_format_one_name_help() {
    let line = format_one(SlashCommand::Spec, 40);
    assert_eq!(line.spans.len(), 3);
}

#[test]
fn test_truncate_help_keeps_short() {
    assert_eq!(truncate_width("short", 10), "short");
}

#[test]
fn test_truncate_help_adds_ellipsis() {
    let out = truncate_width("a very long help string", 10);
    assert!(out.ends_with('\u{2026}'));
    assert!(
        UnicodeWidthStr::width(out.as_str()) <= 10,
        "width overflow: {out}"
    );
}

#[test]
fn test_truncate_help_never_exceeds() {
    // The bug we are guarding against: help must not overflow the popover
    // border. Any help, any max, the result width must be <= max.
    for (help, max) in [
        ("short", 10),
        ("a very long help string", 10),
        ("a very long help string", 38),
        ("a very long help string", 3),
        ("a very long help string", 2),
        ("a very long help string", 0),
    ] {
        let out = truncate_width(help, max);
        assert!(
            UnicodeWidthStr::width(out.as_str()) <= max,
            "max={max} width={}: [{out}]",
            UnicodeWidthStr::width(out.as_str())
        );
    }
}

#[test]
fn test_truncate_zero_max_empty() {
    assert_eq!(truncate_width("anything", 0), "");
}

/// A query that is a command name + space resolves to that command's arg
/// hint (the hint-after-space behavior) — e.g. "resume " → the file/sid hint.
#[test]
fn test_arg_hint_after_space() {
    assert!(
        arg_hint_for("resume ").is_some(),
        "resume + space should yield a hint"
    );
    assert!(
        arg_hint_for("export ").is_some(),
        "export + space should yield a hint"
    );
    assert_eq!(
        arg_hint_for("resume "),
        Some("<file.json | session name | sid>")
    );
}

/// A query that is not an arg-taking command yields no hint (the plain
/// "no match" row shows instead).
#[test]
fn test_arg_hint_none() {
    assert_eq!(arg_hint_for("nope"), None);
    assert_eq!(arg_hint_for("status"), None);
}
