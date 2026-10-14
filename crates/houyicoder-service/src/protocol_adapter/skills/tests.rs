//! Tests for skill entry mapping.

use super::*;
use houyicoder_api::skill::{SkillDescriptor, SkillSnapshot};

/// The /skills entry mapping carries the discovery source (origin) for
/// grouping and the model-invocation gate (invocable) so the pane can flag a
/// skill the model cannot call. The token estimate passes through unchanged.
#[test]
fn test_skill_entries_invocation_gate() {
    let entries = vec![
        SkillSnapshot {
            descriptor: SkillDescriptor {
                name: "pdf-export".into(),
                description: "export chat to pdf".into(),
                when_to_use: None,
                argument_hint: None,
                disable_model_invocation: false,
                user_invocable: true,
                body_token_estimate: 320,
                allowed_tools: Vec::new(),
                allowed_mach_services: Vec::new(),
                allow_app_launch: false,
            },
            origin: "user".into(),
            usage: Default::default(),
        },
        SkillSnapshot {
            descriptor: SkillDescriptor {
                name: "internal-only".into(),
                description: "host-restricted".into(),
                when_to_use: None,
                argument_hint: None,
                disable_model_invocation: true,
                user_invocable: false,
                body_token_estimate: 80,
                allowed_tools: Vec::new(),
                allowed_mach_services: Vec::new(),
                allow_app_launch: false,
            },
            origin: "project".into(),
            usage: Default::default(),
        },
    ];
    let list = skill_entries(entries);
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].name, "pdf-export");
    assert_eq!(list[0].origin, "user");
    assert!(list[0].invocable, "model-invocable skill flagged invocable");
    assert_eq!(list[0].body_token_estimate, 320);
    assert_eq!(list[1].name, "internal-only");
    assert_eq!(list[1].origin, "project");
    assert!(
        !list[1].invocable,
        "disable-model-invocation skill flagged not invocable"
    );
    assert_eq!(list[1].body_token_estimate, 80);
}

#[test]
fn test_skill_entries_empty() {
    assert!(skill_entries(Vec::new()).is_empty());
}
