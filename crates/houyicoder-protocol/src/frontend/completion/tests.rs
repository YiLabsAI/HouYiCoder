//! Tests for this module.

use super::*;

#[test]
fn test_known_words_round_trip() {
    for word in [
        "completed",
        "failed",
        "killed",
        "turn_limit",
        "timed_out",
        "budget",
    ] {
        let status = CompletionStatus::from_word(word);
        assert!(
            !matches!(status, CompletionStatus::Unknown(_)),
            "{word} is a known ending"
        );
        let json = serde_json::to_string(&status).expect("serialize");
        assert_eq!(json, format!("\"{word}\""));
        let back: CompletionStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, status);
        assert_eq!(status.as_str(), word);
    }
}

#[test]
fn test_unknown_word_survives_decode() {
    let status: CompletionStatus = serde_json::from_str("\"abandoned\"").expect("decode");
    assert_eq!(status, CompletionStatus::Unknown("abandoned".to_string()));
    let json = serde_json::to_string(&status).expect("serialize");
    assert_eq!(json, "\"abandoned\"");
}
