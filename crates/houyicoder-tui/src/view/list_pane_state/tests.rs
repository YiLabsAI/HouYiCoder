use super::*;

#[test]
fn test_clamp_within_bounds() {
    let mut s = ListPaneState {
        cursor: 5,
        ..Default::default()
    };
    s.clamp(10);
    assert_eq!(s.cursor, 5, "cursor within range stays");
}

#[test]
fn test_clamp_past_end() {
    let mut s = ListPaneState {
        cursor: 10,
        ..Default::default()
    };
    s.clamp(3);
    assert_eq!(s.cursor, 2, "cursor past end clamps to last index");
}

#[test]
fn test_clamp_empty() {
    let mut s = ListPaneState {
        cursor: 0,
        ..Default::default()
    };
    s.clamp(0);
    assert_eq!(s.cursor, 0, "empty list keeps cursor at zero");
}

#[test]
fn test_move_cursor_down() {
    let mut s = ListPaneState::default();
    s.move_cursor(1, 5);
    assert_eq!(s.cursor, 1);
    s.move_cursor(1, 5);
    assert_eq!(s.cursor, 2);
}

#[test]
fn test_move_cursor_clamps_down() {
    let mut s = ListPaneState {
        cursor: 4,
        ..Default::default()
    };
    s.move_cursor(10, 5);
    assert_eq!(s.cursor, 4, "past the end holds at last row");
}

#[test]
fn test_move_cursor_clamps_up() {
    let mut s = ListPaneState {
        cursor: 0,
        ..Default::default()
    };
    s.move_cursor(-10, 5);
    assert_eq!(s.cursor, 0, "past the start holds at first row");
}

#[test]
fn test_move_cursor_empty_noop() {
    let mut s = ListPaneState {
        cursor: 3,
        ..Default::default()
    };
    s.move_cursor(1, 0);
    assert_eq!(s.cursor, 0, "empty list resets cursor to zero");
}

#[test]
fn test_clear_query() {
    let mut s = ListPaneState {
        query: "test".into(),
        ..Default::default()
    };
    assert!(s.searching());
    s.clear_query();
    assert!(!s.searching());
    assert!(s.query.is_empty());
}

#[test]
fn test_default_no_search() {
    let s = ListPaneState::default();
    assert!(!s.searching());
    assert_eq!(s.cursor, 0);
}

#[test]
fn test_truncate_short() {
    let p = truncate_path("/usr/bin", 32);
    assert_eq!(p, "/usr/bin", "short path unchanged");
}

#[test]
fn test_truncate_truncates_left() {
    let p = truncate_path("/a/b/c/d/e/f/g/h/i/j/k", 10);
    assert!(p.starts_with('\u{2026}'), "long path gets ellipsis prefix");
    assert!(p.chars().count() <= 10, "stays within max width");
    assert!(p.ends_with("k"), "keeps the tail");
}

#[test]
fn test_search_hint_format() {
    let h = search_hint_line("foo");
    assert!(h.contains("foo"), "hint contains the query");
    assert!(h.contains("Esc"), "hint mentions Esc");
}

#[test]
fn test_filter_empty_query() {
    let items = vec!["a", "b", "c"];
    let r = filter_by_query(&items, "", |_| vec![]);
    assert_eq!(r.len(), 3, "empty query returns all");
}

#[test]
fn test_filter_match() {
    let items = vec!["hello", "world", "help"];
    let r = filter_by_query(&items, "hel", |s| vec![s]);
    assert_eq!(r.len(), 2, "hel matches hello + help");
}

#[test]
fn test_filter_case_insensitive() {
    let items = vec!["Hello"];
    let r = filter_by_query(&items, "HEL", |s| vec![s]);
    assert_eq!(r.len(), 1, "case-insensitive match");
}

#[test]
fn test_filter_multi_field() {
    let items = vec![("a", "branch1"), ("b", "branch2")];
    let r = filter_by_query(&items, "branch2", |(_, b)| vec![b]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0], 1);
}

#[test]
fn test_filter_no_match() {
    let items = vec!["a", "b"];
    let r = filter_by_query(&items, "zzz", |s| vec![s]);
    assert!(r.is_empty(), "no match returns empty");
}

#[test]
fn test_truncate_home_prefix() {
    // Synthetic home so the assertion runs regardless of ambient HOME.
    let p = truncate_path_with_home("/home/u/some/repo", 64, "/home/u");
    assert!(p.starts_with("~/"), "HOME prefix becomes ~/");
}

#[test]
fn test_truncate_home_exact() {
    let p = truncate_path_with_home("/home/u", 64, "/home/u");
    assert_eq!(p, "~", "path == HOME becomes ~");
}

#[test]
fn test_truncate_wt_abbrev() {
    let p = truncate_path_with_home("/home/u/.claude/worktrees/feat-x", 64, "/home/u");
    assert!(p.starts_with("~/wt/"), "worktree path abbreviates to ~/wt/");
    assert!(p.contains("feat-x"));
}

#[test]
fn test_truncate_home_boundary() {
    // /home/ubackup must NOT match HOME=/home/u (no slash boundary).
    let p = truncate_path_with_home("/home/ubackup/repo", 64, "/home/u");
    assert!(
        !p.starts_with("~/"),
        "HOME plus suffix without slash must not abbreviate"
    );
}

#[test]
fn test_truncate_max_zero() {
    let p = truncate_path("/anything", 0);
    assert_eq!(p, "", "max=0 returns empty");
}

#[test]
fn test_filter_returns_indices() {
    let items = vec!["a", "bb", "c"];
    let r = filter_by_query(&items, "b", |s| vec![s]);
    assert_eq!(r, vec![1], "returns the index not the value");
}
