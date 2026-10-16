//! Tests for this module.

use super::*;

#[test]
fn test_error_json_byte_exact() {
    let e = ProtocolError::new(ErrorCategory::Internal, "failed", false);
    assert_eq!(
        serde_json::to_string(&e).expect("serialize"),
        r#"{"category":"internal","message":"failed","retriable":false,"correlation":null}"#
    );
}

#[test]
fn test_error_round_trips() {
    let e = ProtocolError::new(ErrorCategory::Unavailable, "worker paused", true)
        .with_correlation("req-7");
    let json = serde_json::to_string(&e).expect("serialize");
    let back: ProtocolError = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.category, ErrorCategory::Unavailable);
    assert_eq!(back.message, "worker paused");
    assert!(back.retriable);
    assert_eq!(back.correlation.as_deref(), Some("req-7"));
}

#[test]
fn test_category_serializes_snake_case() {
    let e = ProtocolError::new(ErrorCategory::ProtocolVersion, "x", false);
    let json = serde_json::to_string(&e).expect("serialize");
    assert!(
        json.contains("protocol_version"),
        "category serializes snake_case for protocol stability: {json}"
    );
}

#[test]
fn test_display_is_message_only() {
    let e = ProtocolError::new(ErrorCategory::Internal, "failed to save settings", false);
    assert_eq!(e.to_string(), "failed to save settings");
}

#[test]
fn test_oversized_frame_category() {
    let too_large = ProtocolError::from(FrameError::TooLarge {
        limit: 8,
        actual: 9,
    });
    assert!(
        too_large.message.contains("9 bytes"),
        "the report names the size that tripped the bound: {}",
        too_large.message
    );
    assert_eq!(too_large.category, ErrorCategory::MessageTooLarge);
    assert!(
        !too_large.retriable,
        "the same bytes fail the same way again"
    );
    let bad_json = ProtocolError::from(FrameError::Json(
        serde_json::from_str::<serde_json::Value>("{").expect_err("bad json"),
    ));
    assert_eq!(bad_json.category, ErrorCategory::InvalidFrame);
}

#[test]
fn test_message_too_large_serializes() {
    let e = ProtocolError::new(ErrorCategory::MessageTooLarge, "x", false);
    let json = serde_json::to_string(&e).expect("serialize");
    assert!(json.contains("message_too_large"), "{json}");
}

#[test]
fn test_frame_error_converts() {
    let frame_err = FrameError::from(serde_json::from_str::<u8>("{").unwrap_err());
    let e = ProtocolError::from(frame_err);
    assert_eq!(e.category, ErrorCategory::InvalidFrame);
    assert!(!e.retriable);
    assert!(!e.message.is_empty(), "carries the frame failure detail");
}
