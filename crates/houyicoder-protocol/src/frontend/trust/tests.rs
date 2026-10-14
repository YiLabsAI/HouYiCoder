//! Tests for this module.

use super::*;

#[test]
fn test_trust_prompt_roundtrips() {
    let prompt = TrustPrompt {
        project_path: "/Users/alice/proj".into(),
        risks: vec![
            TrustRisk {
                kind: "skill_bash".into(),
                name: "commit".into(),
            },
            TrustRisk {
                kind: "hook".into(),
                name: "pre-check".into(),
            },
        ],
    };
    let json = serde_json::to_string(&prompt).unwrap();
    let back: TrustPrompt = serde_json::from_str(&json).unwrap();
    assert_eq!(back.project_path, "/Users/alice/proj");
    assert_eq!(back.risks.len(), 2);
    assert_eq!(back.risks[0].kind, "skill_bash");
    assert_eq!(back.risks[1].name, "pre-check");
}

/// risks omits from the wire when empty, and a prompt without the field
/// deserializes to an empty list (older client or server compatibility).
#[test]
fn test_trust_prompt_omits_risks() {
    let prompt = TrustPrompt {
        project_path: "/proj".into(),
        risks: Vec::new(),
    };
    let json = serde_json::to_string(&prompt).unwrap();
    assert!(
        !json.contains("risks"),
        "empty risks must be omitted from the wire: {json}"
    );
    let back: TrustPrompt = serde_json::from_str(r#"{"project_path":"/proj"}"#).unwrap();
    assert!(back.risks.is_empty(), "missing risks defaults to empty");
}

/// TrustAccept carries only the boolean; the wire stays minimal so the
/// client reply is unambiguous in both directions.
#[test]
fn test_trust_accept_roundtrips() {
    let accept = TrustAccept { accepted: true };
    let back: TrustAccept = serde_json::from_str(&serde_json::to_string(&accept).unwrap()).unwrap();
    assert!(back.accepted);
    let decline = TrustAccept { accepted: false };
    let back: TrustAccept =
        serde_json::from_str(&serde_json::to_string(&decline).unwrap()).unwrap();
    assert!(!back.accepted);
}
