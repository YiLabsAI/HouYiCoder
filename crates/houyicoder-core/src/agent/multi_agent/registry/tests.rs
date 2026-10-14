//! Tests for this module.

use std::collections::HashSet;

use super::{
    AgentError, AgentRegistry, BuiltInRegistry, ResolveCtx, built_in_all, built_in_general_purpose,
};

/// A registered built-in must resolve to its definition, unchanged.
#[test]
fn test_registry_resolves_builtin() {
    let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
    let def = reg
        .resolve("general-purpose", &ResolveCtx::default())
        .expect("registered type must resolve");
    assert_eq!(def.subagent_type, "general-purpose");
    assert!(!def.when_to_use.is_empty());
}

/// An unregistered type must surface the available set so the caller can
/// report it, rather than panicking or returning a bare not-found.
#[test]
fn test_unregistered_lists_available() {
    let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
    let err = reg
        .resolve("nonexistent", &ResolveCtx::default())
        .expect_err("unregistered type must error");
    match err {
        AgentError::NotFound { available, .. } => {
            assert!(available.contains(&"general-purpose".to_string()));
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

/// A deny hit must return a distinct error from not-registered. The two
/// causes drive different caller behavior: an absent type is a usage
/// mistake; a denied type is a policy decision that may need escalation.
#[test]
fn test_denied_returns_permission_denied() {
    let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
    let ctx = ResolveCtx::default().with_denied(["general-purpose".to_string()]);
    let err = reg
        .resolve("general-purpose", &ctx)
        .expect_err("denied type must error");
    assert!(matches!(err, AgentError::PermissionDenied { .. }));
}

/// A type that is denied but not registered must be NotFound, not
/// PermissionDenied. Denying a non-existent name would leak that the
/// name is on the deny list and report a policy decision for a type the
/// caller cannot use anyway.
#[test]
fn test_deny_skips_unregistered() {
    let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
    let ctx = ResolveCtx::default().with_denied(["ghost".to_string()]);
    let err = reg
        .resolve("ghost", &ctx)
        .expect_err("unregistered type must error");
    assert!(matches!(err, AgentError::NotFound { .. }));
}

/// list returns every registered definition so the caller can build the
/// catalog injection or a user-facing roster.
#[test]
fn test_list_returns_all_registered() {
    let reg = BuiltInRegistry::from_agents(vec![built_in_general_purpose()]);
    let all = reg.list();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].subagent_type, "general-purpose");
}

/// explore is read-only: the file-mutating tools and the sub-agent tool
/// are denied, and it runs on the fast model tier (search is throughput,
/// not deep reasoning).
#[test]
fn test_explore_disallows_write() {
    let def = super::built_in_explore();
    assert!(def.disallowed_tools.contains(&"write".to_string()));
    assert!(def.disallowed_tools.contains(&"edit".to_string()));
    assert!(def.disallowed_tools.contains(&"agent".to_string()));
    assert_eq!(def.model.as_deref(), Some("Flash"));
}

#[test]
fn test_plan_model_is_max() {
    assert_eq!(super::built_in_plan().model.as_deref(), Some("Max"));
}

#[test]
fn test_verify_model_is_max() {
    assert_eq!(super::built_in_verify().model.as_deref(), Some("Max"));
}

#[test]
fn test_guide_uses_flash() {
    assert_eq!(super::built_in_code_guide().model.as_deref(), Some("Flash"));
}

/// built_in_all returns the five built-ins in a stable order so catalog
/// injection is byte-stable across sessions.
#[test]
fn test_builtin_count_five() {
    let all = built_in_all();
    assert_eq!(all.len(), 5);
    let types: Vec<&str> = all.iter().map(|d| d.subagent_type.as_str()).collect();
    assert_eq!(
        types,
        ["general-purpose", "explore", "plan", "verify", "code-guide"]
    );
}

/// Read-only search agents fan out frequently and never act on project
/// rules, so their user context drops the project memory; write-bearing
/// agents keep it.
#[test]
fn test_explore_plan_omit_memory() {
    assert!(super::built_in_explore().omit_project_context);
    assert!(super::built_in_plan().omit_project_context);
    assert!(!super::built_in_general_purpose().omit_project_context);
    assert!(!super::built_in_verify().omit_project_context);
}

fn built_in() -> BuiltInRegistry {
    BuiltInRegistry::from_agents(super::built_in_all())
}

#[test]
fn test_agent_directory_lists_sorted() {
    let reg = built_in();
    let dir = super::agent_directory_section(&reg, &HashSet::new()).expect("non-empty");
    // Deterministic alphabetical order by type.
    let code_pos = dir.find("- code-guide:").unwrap();
    let explore_pos = dir.find("- explore:").unwrap();
    let general_pos = dir.find("- general-purpose:").unwrap();
    let plan_pos = dir.find("- plan:").unwrap();
    assert!(code_pos < explore_pos);
    assert!(explore_pos < general_pos);
    assert!(general_pos < plan_pos);
}

#[test]
fn test_agent_directory_filters_denied() {
    let reg = built_in();
    let mut denied = HashSet::new();
    denied.insert("explore".to_string());
    let dir = super::agent_directory_section(&reg, &denied).expect("non-empty");
    assert!(!dir.contains("- explore:"), "denied type must not appear");
    assert!(dir.contains("- plan:"), "non-denied type stays");
}

#[test]
fn test_agent_directory_stable_bytes() {
    let reg = built_in();
    let a = super::agent_directory_section(&reg, &HashSet::new());
    let b = super::agent_directory_section(&reg, &HashSet::new());
    assert_eq!(a, b, "same registry + denied must yield identical bytes");
}

#[test]
fn test_agent_directory_all_denied() {
    let reg = built_in();
    let all: HashSet<String> = super::built_in_all()
        .into_iter()
        .map(|d| d.subagent_type)
        .collect();
    assert!(super::agent_directory_section(&reg, &all).is_none());
}

#[test]
fn test_format_agent_line() {
    let mut def = super::built_in_explore();
    def.tools = Some(vec!["read".into(), "grep".into()]);
    let line = super::format_agent_line(&def);
    assert!(line.starts_with("- explore:"));
    assert!(line.contains("(Tools: read, grep)"));
}
