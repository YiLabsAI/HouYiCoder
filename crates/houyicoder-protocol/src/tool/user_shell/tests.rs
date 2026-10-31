//! Tests for this module.

use super::*;

/// An event id in its display spelling, the shape the call-id constructor is
/// handed.
const EVENT_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

#[test]
fn test_generated_id_is_recognized() {
    let call_id = user_shell_call_id(EVENT_ID);
    assert_eq!(call_id, "shell-01ARZ3NDEKTSV4RRFFQ69G5FAV");
    assert!(is_user_shell_call_id(&call_id));
}

#[test]
fn test_provider_id_unmatched() {
    // Provider call ids are free-form text that can hold the prefix.
    for call_id in [
        "shell-1",
        "shell-call_abc",
        "call-shell-",
        "bash-01ARZ3NDEKTSV4RRFFQ69G5FAV",
    ] {
        assert!(!is_user_shell_call_id(call_id), "{call_id} must not match");
    }
}

/// A model call that happens to spell the reserved shape keeps its text under
/// a prefix that breaks the shape, so every later reader sees a model call.
#[test]
fn test_model_id_distinct() {
    let reserved = user_shell_call_id(EVENT_ID);
    let normalized = model_call_id(&reserved);
    assert_eq!(normalized, format!("model-{reserved}"));
    assert!(!is_user_shell_call_id(&normalized));
}

/// Every other id passes through unchanged: the normalization only touches an
/// id that would otherwise be read as the user's own.
#[test]
fn test_model_id_unchanged() {
    for call_id in ["call_abc", "shell-1"] {
        assert_eq!(model_call_id(call_id), call_id);
    }
}

/// Two distinct calls in one response keep distinct ids: renaming the reserved
/// shape must not land on another id the provider already sent, because every
/// consumer keys results by id.
#[test]
fn test_colliding_ids_stay_distinct() {
    let reserved = user_shell_call_id(EVENT_ID);
    let occupied = model_call_id(&reserved);
    let mut taken = HashSet::new();
    let renamed = unique_model_call_id(&reserved, &mut taken);
    let sibling = unique_model_call_id(&occupied, &mut taken);
    assert_ne!(renamed, sibling);
    assert_eq!(renamed, occupied, "the first id keeps the renamed text");
    assert!(!is_user_shell_call_id(&renamed));
    assert_eq!(
        sibling,
        format!("{occupied}-1"),
        "the collision takes a suffix that also leaves the reserved shape"
    );
}

/// A response with no collision keeps every id as the provider spelled it,
/// so normalization stays invisible on the ordinary path.
#[test]
fn test_unique_ids_unchanged() {
    let mut taken = HashSet::new();
    assert_eq!(unique_model_call_id("call_a", &mut taken), "call_a");
    assert_eq!(unique_model_call_id("call_b", &mut taken), "call_b");
}

#[test]
fn test_rejects_malformed_suffix() {
    // A suffix of the wrong length, a lowercase spelling, or a character
    // outside the event id alphabet is not one.
    for suffix in [
        "01ARZ3NDEKTSV4RRFFQ69G5FA",
        "01ARZ3NDEKTSV4RRFFQ69G5FAVV",
        "01arz3ndektsv4rrffq69g5fav",
        "01ARZ3NDEKTSV4RRFFQ69G5FAI",
        "01ARZ3NDEKTSV4RRFFQ69G5FAL",
    ] {
        let call_id = format!("shell-{suffix}");
        assert!(!is_user_shell_call_id(&call_id), "{call_id} must not match");
    }
}
