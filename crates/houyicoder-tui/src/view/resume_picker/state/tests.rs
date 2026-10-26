//! The picker's filter, its selection wrap, and its relative-time buckets.

use super::*;

fn row(sid: &str, title: &str, ts: u64) -> SessionRow {
    SessionRow {
        sid_str: sid.into(),
        title: title.into(),
        cwd_basename: "repo".into(),
        last_active: ts,
        ..Default::default()
    }
}

impl SessionPickerState {
    fn query_filter(&mut self, q: &str) -> Vec<&SessionRow> {
        self.query = q.into();
        self.sel = 0;
        self.filtered()
    }
}

#[test]
fn test_empty_query_returns_all() {
    let p = SessionPickerState {
        rows: vec![row("a", "alpha", 1), row("b", "beta", 2)],
        ..Default::default()
    };
    assert_eq!(p.filtered().len(), 2);
}

/// Case is folded in every script, not only the ASCII range, so a title
/// carrying an accented word is found by the query typed in lower case.
#[test]
fn test_filter_folds_case() {
    let mut p = SessionPickerState {
        rows: vec![row("a", "Überblick", 1), row("b", "plain", 2)],
        ..Default::default()
    };
    assert_eq!(p.query_filter("über").len(), 1);
    assert_eq!(p.query_filter("ÜBER").len(), 1);
    assert_eq!(p.query_filter("PLAIN").len(), 1);
}

#[test]
fn test_query_matches_sid_title() {
    let mut p = SessionPickerState {
        rows: vec![
            row("11111111-1111-1111-1111-111111111111", "alpha", 1),
            row("22222222-2222-2222-2222-222222222222", "beta login", 2),
        ],
        ..Default::default()
    };
    assert_eq!(p.query_filter("1111").len(), 1);
    assert_eq!(p.query_filter("log").len(), 1);
    assert_eq!(p.query_filter("zzz").len(), 0);
}

#[test]
fn test_selection_wraps() {
    let mut p = SessionPickerState {
        rows: vec![row("a", "x", 1), row("b", "y", 2)],
        ..Default::default()
    };
    p.open();
    assert_eq!(p.sel, 0);
    p.next();
    assert_eq!(p.sel, 1);
    p.next();
    assert_eq!(p.sel, 0);
    p.prev();
    assert_eq!(p.sel, 1);
}

#[test]
fn test_push_resets_selection() {
    let mut p = SessionPickerState {
        rows: vec![row("a", "x", 1), row("b", "y", 2)],
        ..Default::default()
    };
    p.open();
    p.next();
    assert_eq!(p.sel, 1);
    p.push('a');
    assert_eq!(p.sel, 0);
}

#[test]
fn test_age_column_buckets() {
    assert_eq!(age_column(0, 0), "now");
    assert_eq!(age_column(0, 30), "now");
    assert_eq!(age_column(0, 120), "2m");
    assert_eq!(age_column(0, 7200), "2h");
    assert_eq!(age_column(0, 3 * 86_400), "3d");
}

/// Both leading columns keep their width for any age and any size, so the
/// title starts at one column on every row. An unbounded day count or a size
/// past the scale pushed the title right on exactly the old sessions.
#[test]
fn test_row_columns_stay_narrow() {
    let day: u64 = 86_400;
    let ages = [
        0,
        59,
        60,
        3599,
        3600,
        86_399,
        day,
        364 * day,
        365 * day,
        40 * 365 * day,
        u64::MAX,
    ];
    for age in ages {
        let text = age_column(0, age);
        assert!(
            text.chars().count() <= 4,
            "time column overflows for age {age}: {text}"
        );
    }
    let sizes = [
        0,
        1023,
        1024,
        1024 * 1024 - 1,
        1024 * 1024 * 1024,
        3 * 1024 * 1024 * 1024,
        10 * 1024 * 1024 * 1024 * 1024,
        u64::MAX,
    ];
    for bytes in sizes {
        let text = format_size(bytes);
        assert!(
            text.chars().count() <= 5,
            "size column overflows for {bytes} bytes: {text}"
        );
    }
    assert_eq!(age_column(0, 3 * 365 * day), "3y");
    assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3G");
    assert_eq!(format_size(10 * 1024 * 1024 * 1024 * 1024), "10T");
}

mod open;
