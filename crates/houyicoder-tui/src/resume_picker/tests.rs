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
fn test_relative_time_buckets() {
    assert_eq!(relative_time(0, 0), "now");
    assert_eq!(relative_time(0, 30), "now");
    assert_eq!(relative_time(0, 120), "2m");
    assert_eq!(relative_time(0, 7200), "2h");
    assert_eq!(relative_time(0, 3 * 86_400), "3d");
}

mod open;
