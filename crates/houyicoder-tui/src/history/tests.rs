use super::*;
use crate::input::InputField;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::process::id;

fn nav() -> HistoryNav {
    HistoryNav::default()
}

fn field(s: &str) -> InputField {
    let mut f = InputField::new();
    f.set(s.to_string());
    f
}

#[test]
fn test_up_recalls_older() {
    let mut n = nav();
    n.cache = vec!["newer".into(), "older".into()];
    n.loaded = true;
    let mut f = field("");
    assert!(n.up(&mut f, "p", "s"));
    assert_eq!(f.value(), "newer");
    assert_eq!(f.cursor(), 0);
    assert!(n.up(&mut f, "p", "s"));
    assert_eq!(f.value(), "older");
}

#[test]
fn test_up_stops_at_oldest() {
    let mut n = nav();
    n.cache = vec!["only".into()];
    n.loaded = true;
    let mut f = field("");
    assert!(n.up(&mut f, "p", "s"));
    assert!(!n.up(&mut f, "p", "s"));
    assert_eq!(f.value(), "only");
}

#[test]
fn test_down_restores_draft() {
    let mut n = nav();
    n.cache = vec!["old".into()];
    n.loaded = true;
    let mut f = field("my draft");
    assert!(n.up(&mut f, "p", "s"));
    assert_eq!(f.value(), "old");
    assert!(n.down(&mut f));
    assert_eq!(f.value(), "my draft");
    assert_eq!(f.cursor(), "my draft".len());
}

#[test]
fn test_down_clears_no_draft() {
    let mut n = nav();
    n.cache = vec!["old".into()];
    n.loaded = true;
    let mut f = field("");
    assert!(n.up(&mut f, "p", "s"));
    assert!(n.down(&mut f));
    assert_eq!(f.value(), "");
}

#[test]
fn test_down_noop_idle() {
    let mut n = nav();
    let mut f = field("text");
    assert!(!n.down(&mut f));
}

#[test]
fn test_draft_skipped_empty() {
    let mut n = nav();
    n.cache = vec!["old".into()];
    n.loaded = true;
    let mut f = field("   ");
    assert!(n.up(&mut f, "p", "s"));
    assert!(n.down(&mut f));
    assert_eq!(f.value(), "");
}

#[test]
fn test_remove_last_soft_skips() {
    let mut n = nav();
    n.last_added = Some(42);
    n.remove_last();
    assert!(n.skip.contains(&42));
    assert!(n.last_added.is_none());
    n.remove_last();
    assert_eq!(n.skip.len(), 1);
}

#[test]
fn test_reset_clears_skip() {
    let mut n = nav();
    n.last_added = Some(1);
    n.skip.insert(1);
    n.cache = vec!["x".into()];
    n.index = 1;
    n.loaded = true;
    n.reset();
    assert!(n.skip.is_empty());
    assert!(n.last_added.is_none());
    assert_eq!(n.index, 0);
    assert!(!n.loaded);
}

#[test]
fn test_add_read_roundtrip() {
    let dir = temp_dir().join(format!("houyi-hist-rt-{}-{}", id(), now_ts()));
    create_dir_all(&dir).unwrap();
    let path = dir.join("history.jsonl");
    add("hello world", "proj", "sess", &path);
    add("second", "proj", "sess", &path);
    let skip = HashSet::new();
    let got = read("proj", "sess", &skip, &path);
    assert_eq!(got, vec!["second".to_string(), "hello world".to_string()]);
    let got_other = read("other", "sess", &skip, &path);
    assert!(got_other.is_empty());
    remove_dir_all(&dir).ok();
}

#[test]
fn test_skip_set_drops_aborted() {
    let dir = temp_dir().join(format!("houyi-hist-skip-{}-{}", id(), now_ts()));
    create_dir_all(&dir).unwrap();
    let path = dir.join("history.jsonl");
    let _keep = add("keep", "proj", "sess", &path);
    let abort = add("abort", "proj", "sess", &path);
    let mut skip = HashSet::new();
    skip.insert(abort);
    let got = read("proj", "sess", &skip, &path);
    assert_eq!(got, vec!["keep".to_string()]);
    remove_dir_all(&dir).ok();
}
