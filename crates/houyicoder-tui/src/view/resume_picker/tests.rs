use super::*;

/// A row renders the time, the title, and the cwd basename, and never the
/// sid (the design density decision).
#[test]
fn test_format_row_layout() {
    let row = crate::resume_picker::SessionRow {
        sid_str: "secret-sid".into(),
        title: "login flow".into(),
        cwd_basename: "app".into(),
        last_active: 120,
        ..Default::default()
    };
    let line = format_line(&row, 240, 80);
    let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(rendered.contains("2m"), "time column: {rendered}");
    assert!(rendered.contains("login flow"), "title: {rendered}");
    assert!(rendered.contains("app"), "cwd basename: {rendered}");
    assert!(
        !rendered.contains("secret-sid"),
        "sid must not show: {rendered}"
    );
}

/// Truncation appends an ellipsis when the title exceeds the column budget.
#[test]
fn test_truncate_appends_ellipsis() {
    let s = "abcdefghij";
    assert_eq!(truncate_width(s, 10), "abcdefghij");
    assert_eq!(truncate_width(s, 0), "");
    let out = truncate_width(s, 5);
    assert!(out.ends_with('\u{2026}'));
    assert!(
        unicode_width::UnicodeWidthStr::width(out.as_str()) <= 5,
        "width overflow: {out}"
    );
}
