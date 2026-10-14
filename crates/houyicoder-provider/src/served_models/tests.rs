mod mock;

use super::*;

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
