//! Tests for this module.

use super::*;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::path::PathBuf;
use std::process::id;

fn t0() -> Instant {
    Instant::now()
}

/// A single event fires CheckStability only after the debounce gap, and
/// Reload only after the stability window.
#[test]
fn test_reload_after_stability() {
    let mut s = ReloadScheduler::new();
    let start = t0();
    s.on_event(start, PathBuf::from("a/SKILL.md"));
    assert!(s.poll(start + Duration::from_millis(100)).is_none());
    let act = s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(1));
    assert!(matches!(act, Some(Action::CheckStability(_))));
    s.confirm_stable(start + RELOAD_DEBOUNCE + Duration::from_millis(2), true);
    assert!(
        s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(500))
            .is_none()
    );
    let act = s.poll(start + RELOAD_DEBOUNCE + WRITE_STABILITY + Duration::from_millis(10));
    assert_eq!(act, Some(Action::Reload));
    assert!(
        s.poll(start + RELOAD_DEBOUNCE + WRITE_STABILITY + Duration::from_secs(2))
            .is_none()
    );
}

/// Burst events extend the deadline so only the last quiet gap fires.
#[test]
fn test_burst_extends_deadline() {
    let mut s = ReloadScheduler::new();
    let start = t0();
    s.on_event(start, PathBuf::from("a/SKILL.md"));
    s.on_event(
        start + RELOAD_DEBOUNCE - Duration::from_millis(10),
        PathBuf::from("b/SKILL.md"),
    );
    assert!(
        s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(1))
            .is_none()
    );
    let act = s.poll(start + RELOAD_DEBOUNCE * 2 + Duration::from_millis(1));
    match act {
        Some(Action::CheckStability(paths)) => assert_eq!(paths.len(), 2),
        other => panic!("expected CheckStability, got {other:?}"),
    }
}

/// A still-changing file resets the stability window so a multi-burst
/// write only reloads after the final burst settles.
#[test]
fn test_changing_resets_stability() {
    let mut s = ReloadScheduler::new();
    let start = t0();
    s.on_event(start, PathBuf::from("a/SKILL.md"));
    drop(s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(1)));
    s.confirm_stable(start + RELOAD_DEBOUNCE + Duration::from_millis(2), true);
    s.confirm_stable(start + RELOAD_DEBOUNCE + Duration::from_millis(600), false);
    s.confirm_stable(start + RELOAD_DEBOUNCE + Duration::from_millis(700), true);
    assert!(
        s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(800))
            .is_none()
    );
    let act = s.poll(start + RELOAD_DEBOUNCE + WRITE_STABILITY + Duration::from_millis(800));
    assert_eq!(act, Some(Action::Reload));
}

/// An event during stability checking restarts debounce.
#[test]
fn test_event_restarts_debounce() {
    let mut s = ReloadScheduler::new();
    let start = t0();
    s.on_event(start, PathBuf::from("a/SKILL.md"));
    drop(s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(1)));
    s.on_event(
        start + RELOAD_DEBOUNCE + Duration::from_millis(100),
        PathBuf::from("b/SKILL.md"),
    );
    assert!(
        s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(200))
            .is_none()
    );
    let act = s.poll(start + RELOAD_DEBOUNCE * 2 + Duration::from_millis(101));
    match act {
        Some(Action::CheckStability(paths)) => assert_eq!(paths.len(), 2),
        other => panic!("expected CheckStability, got {other:?}"),
    }
}

/// Readable roots: swap regardless of count (empty is legitimate — the
/// user deleted the last skill).
#[test]
fn test_readable_roots_swap() {
    assert!(should_swap(5, 0, true), "delete all when roots readable");
    assert!(should_swap(5, 3, true));
    assert!(should_swap(0, 0, true));
}

/// Unreadable root: a shrink is suspect (partial read failure) — keep;
/// a non-shrink is fine to swap.
#[test]
fn test_unreadable_keeps_shrink() {
    assert!(
        !should_swap(5, 0, false),
        "total loss on unreadable root: keep"
    );
    assert!(
        !should_swap(5, 3, false),
        "partial shrink on unreadable root: keep"
    );
    assert!(should_swap(5, 5, false), "no shrink: swap");
    assert!(should_swap(3, 5, false), "growth: swap");
}

/// A real skills directory is Deep; a family directory without a skills
/// subdirectory is Shallow; a family directory that does not exist is
/// skipped.
#[test]
fn test_watch_roots_depth() {
    let tmp = temp_dir().join(format!("skill-watch-{}", id()));
    drop(remove_dir_all(&tmp));
    create_dir_all(tmp.join(".houyicoder").join("skills")).unwrap();
    create_dir_all(tmp.join(".claude")).unwrap();
    let roots = watch_roots(Some(&tmp), None);
    let deep: Vec<_> = roots
        .iter()
        .filter(|(_, d)| *d == WatchDepth::Deep)
        .map(|(p, _)| p.clone())
        .collect();
    let shallow: Vec<_> = roots
        .iter()
        .filter(|(_, d)| *d == WatchDepth::Shallow)
        .map(|(p, _)| p.clone())
        .collect();
    assert!(
        deep.iter().any(|p| p.ends_with("skills")),
        "skills dir watched Deep: {deep:?}"
    );
    assert!(
        shallow.iter().any(|p| p.ends_with(".claude")),
        "family dir without skills watched Shallow: {shallow:?}"
    );
    assert!(
        !roots.iter().any(|(p, _)| p.ends_with(".agents")),
        "absent family skipped"
    );
    drop(remove_dir_all(&tmp));
}

/// Overlapping walk-up roots do not double-register the same canonical
/// skills directory.
#[test]
fn test_watch_roots_dedup() {
    let tmp = temp_dir().join(format!("skill-watch-dedup-{}", id()));
    drop(remove_dir_all(&tmp));
    create_dir_all(tmp.join(".houyicoder").join("skills")).unwrap();
    let roots = watch_roots(Some(&tmp), None);
    let skills_entries: Vec<_> = roots
        .iter()
        .filter(|(p, _)| p.ends_with("skills"))
        .map(|(p, _)| p.clone())
        .collect();
    assert_eq!(
        skills_entries.len(),
        1,
        "deduped to one: {skills_entries:?}"
    );
    drop(remove_dir_all(&tmp));
}
