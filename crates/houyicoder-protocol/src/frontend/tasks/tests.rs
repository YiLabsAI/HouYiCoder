//! Tests for this module.

use super::*;

#[test]
fn test_known_states_round_trip() {
    for word in ["running", "finished", "failed", "killed", "timed_out"] {
        let status = BackgroundTaskStatus::from_word(word);
        assert!(
            !matches!(status, BackgroundTaskStatus::Unknown(_)),
            "{word} is a known state"
        );
        let json = serde_json::to_string(&status).expect("serialize");
        assert_eq!(json, format!("\"{word}\""));
        let back: BackgroundTaskStatus = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, status);
        assert_eq!(status.as_str(), word);
    }
    assert!(!BackgroundTaskStatus::Running.is_terminal());
    assert!(BackgroundTaskStatus::Killed.is_terminal());
    assert!(
        !BackgroundTaskStatus::Unknown("paused".to_string()).is_terminal(),
        "a state this build cannot name is not taken as an ending"
    );
}

#[test]
fn test_unknown_state_survives_decode() {
    let status: BackgroundTaskStatus = serde_json::from_str("\"paused\"").expect("decode");
    assert_eq!(status, BackgroundTaskStatus::Unknown("paused".to_string()));
    let json = serde_json::to_string(&status).expect("serialize");
    assert_eq!(json, "\"paused\"");
}

#[test]
fn test_snapshot_round_trips() {
    let task = BackgroundTaskSnapshot {
        id: BackgroundTaskId(7),
        status: BackgroundTaskStatus::Running,
        command: "cargo test".to_string(),
        elapsed_ms: 12_500,
        exit_code: None,
    };
    let json = serde_json::to_string(&task).expect("serialize");
    let back: BackgroundTaskSnapshot = serde_json::from_str(&json).expect("decode");
    assert_eq!(back, task);
    assert_eq!(task.id.label(), "#7");
}

#[test]
fn test_snapshot_minimal_decodes() {
    let json =
        r#"{"id":2,"status":"finished","command":"make check","elapsed_ms":1000,"exit_code":0}"#;
    let task: BackgroundTaskSnapshot = serde_json::from_str(json).expect("decode");
    assert_eq!(task.exit_code, Some(0));
}
