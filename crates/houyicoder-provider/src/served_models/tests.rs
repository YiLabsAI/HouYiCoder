mod mock;

use super::*;
use houyicoder_config::load_ids_at;
use std::env::temp_dir;
use std::fs;
use std::process::id;

/// A write followed by a read returns the ids that went in, and leaves no temp
/// behind, which is the contract the atomic replace holds across two writers
/// and a crash. Exercises the write path without a network mock so the coverage
/// build sees it run.
#[test]
fn test_write_cache_roundtrip() {
    let dir = temp_dir().join(format!("served-cache-{}", id()));
    fs::create_dir_all(&dir).expect("a temp dir for the cache");
    let cache = dir.join("cache.json");
    write_cache(&cache, &["alpha".into(), "beta".into()]);
    let ids = load_ids_at(&cache);
    assert_eq!(
        ids,
        vec!["alpha", "beta"],
        "the cache holds what was written"
    );
    let leftover: Vec<_> = fs::read_dir(&dir)
        .expect("list the dir")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".cache.json.")
        })
        .collect();
    assert!(leftover.is_empty(), "no temp file is left behind");
    fs::remove_dir_all(&dir).ok();
}

/// A replace that cannot rename a temp over its target is swallowed rather than
/// panicking, so a bad target path never stops the startup that fired the write.
/// The target is a directory, so a file cannot rename over it.
#[test]
fn test_write_cache_handles_failure() {
    let root = temp_dir().join(format!("served-fail-{}", id()));
    fs::create_dir_all(&root).expect("a root for the bad target");
    let target = root.join("target");
    fs::create_dir_all(&target).expect("the target is a directory");
    write_cache(&target, &["x".into()]);
    assert!(target.is_dir(), "the directory target is untouched");
    fs::remove_dir_all(&root).ok();
}

#[test]
fn test_parse_extracts_data_ids() {
    let body: Value = serde_json::json!({
        "data": [
            {"id": "qwen3.7-max", "object": "model"},
            {"id": "glm-5.2", "object": "model"},
        ]
    });
    assert_eq!(parse_response(&body), vec!["qwen3.7-max", "glm-5.2"]);
}

#[test]
fn test_parse_dedups_case_insensitive() {
    let body: Value = serde_json::json!({
        "data": [{"id": "qwen3.7-max"}, {"id": "QWEN3.7-MAX"}, {"id": "glm-5.2"}]
    });
    let ids = parse_response(&body);
    assert_eq!(ids.len(), 2, "dedup case-insensitive");
    assert!(ids.contains(&"qwen3.7-max".to_string()));
}

#[test]
fn test_parse_drops_blank_ids() {
    let body: Value = serde_json::json!({
        "data": [{"id": "  "}, {"object": "model"}, {"id": "glm-5.2"}]
    });
    let ids = parse_response(&body);
    assert_eq!(ids, vec!["glm-5.2"]);
}

#[test]
fn test_parse_empty_without_data() {
    let body: Value = serde_json::json!({"object": "list"});
    assert!(parse_response(&body).is_empty());
}

#[test]
fn test_parse_sorts_longest_id() {
    let body: Value = serde_json::json!({
        "data": [{"id": "qwen3"}, {"id": "qwen3.7-max"}, {"id": "glm"}]
    });
    let ids = parse_response(&body);
    // Longest first so the substring match prefers the most specific.
    assert_eq!(ids[0], "qwen3.7-max");
}
