//! Tests for this module.

use super::*;

#[test]
fn test_exists_match_case_insensitive() {
    assert!(exists_in(&["qwen3.7-max".into()], "QWEN3.7-MAX"));
    assert!(!exists_in(&["qwen3.7-max".into()], "glm-5.2"));
}

#[test]
fn test_exists_substring_longest_id() {
    // "qwen3.7-max" contains both "qwen3" and "qwen3.7"; the longest-id
    // sort does not let a short non-matching id shadow a longer match.
    let ids = vec!["qwen3".into(), "qwen3.7".into()];
    assert!(exists_in(&ids, "qwen3.7-max"));
    assert!(!exists_in(&ids, "glm-5.2"));
}

#[test]
fn test_exists_empty_ids_false() {
    assert!(!exists_in(&[], "qwen3.7-max"));
}

#[test]
fn test_missing_file_is_empty() {
    let ids = load_ids_at(Path::new("/nonexistent/v1-models-test.json"));
    assert!(ids.is_empty());
}

#[test]
fn test_corrupt_is_empty() {
    let path = temp_dir().join(format!("v1-corrupt-{}.json", id()));
    write(&path, "not json").unwrap();
    assert!(load_ids_at(&path).is_empty());
    drop(remove_file(&path));
}

#[test]
fn test_reads_cached_ids() {
    let path = temp_dir().join(format!("v1-read-{}.json", id()));
    write(
        &path,
        r#"{"ids":["qwen3.7-max","glm-5.2"],"timestamp":1700000000}"#,
    )
    .unwrap();
    let ids = load_ids_at(&path);
    assert_eq!(ids, vec!["qwen3.7-max", "glm-5.2"]);
    drop(remove_file(&path));
}

#[test]
fn test_served_models_round_trips() {
    let s = ServedModels {
        ids: vec!["qwen3.7-max".into(), "glm-5.2".into()],
        timestamp: 1_700_000_000,
    };
    let json = serde_json::to_string(&s).unwrap();
    let back: ServedModels = serde_json::from_str(&json).unwrap();
    assert_eq!(s, back);
}
