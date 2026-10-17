//! Tests for this module.

use super::{AgentEntry, AgentPrompt};

fn entry() -> AgentEntry {
    AgentEntry {
        subagent_type: "plan".into(),
        when_to_use: "designs a plan".into(),
        tools: None,
        disallowed_tools: vec!["write".into()],
        model: Some("Max".into()),
        effort: None,
        system_prompt: AgentPrompt::Owned("you are an architect".into()),
    }
}

#[test]
fn test_round_trips_camel_case() {
    let text = serde_json::to_string(&entry()).expect("serialize");
    assert!(text.contains("\"subagentType\":\"plan\""), "{text}");
    assert!(text.contains("\"disallowedTools\":[\"write\"]"), "{text}");
    let back: AgentEntry = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back, entry());
}

/// An absent tool set and an absent model are inherited, not empty: they must
/// survive a round trip as None so the pane can say "inherits" rather than
/// showing a measured nothing.
#[test]
fn test_absent_fields_stay_absent() {
    let text = serde_json::to_string(&entry()).expect("serialize");
    assert!(!text.contains("\"tools\""), "{text}");
    assert!(!text.contains("\"effort\""), "{text}");
    let back: AgentEntry = serde_json::from_str(&text).expect("deserialize");
    assert!(back.tools.is_none());
    assert!(back.effort.is_none());
}

/// Inheriting the caller's prompt survives the round trip as a variant, so
/// the pane's wording is driven by the declared source rather than by an
/// absent body.
#[test]
fn test_inherited_prompt_round_trips() {
    let mut e = entry();
    e.system_prompt = AgentPrompt::InheritParent;
    let text = serde_json::to_string(&e).expect("serialize");
    assert!(text.contains("\"source\":\"inheritParent\""), "{text}");
    let back: AgentEntry = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back.system_prompt, AgentPrompt::InheritParent);
}

/// The owned variant carries the body under the content tag, so a reader
/// never has to guess whether a present string means the prompt was declared.
#[test]
fn test_owned_prompt_carries_body() {
    let text = serde_json::to_string(&entry()).expect("serialize");
    assert!(text.contains("\"source\":\"owned\""), "{text}");
    assert!(text.contains("\"body\":\"you are an architect\""), "{text}");
}
