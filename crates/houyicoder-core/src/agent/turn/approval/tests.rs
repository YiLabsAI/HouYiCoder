//! Tests for this module.

use super::*;
use houyicoder_api::skill::{SkillFamily, SkillProvenance, SkillSource};
use std::env;
use std::fs;
use std::process;

fn user_subject(skill: &str) -> GrantSubject {
    SkillSource::new(SkillFamily::Houyi, SkillProvenance::UserHome).grant_subject(skill)
}

#[test]
fn test_entitlement_grants_services() {
    let dir = env::temp_dir().join(format!("houyi-entitlement-grant-{}", process::id()));
    let _ = fs::remove_dir_all(&dir).is_ok();
    fs::create_dir_all(&dir).expect("mkdir grant test");
    let path = dir.join("skill-grants.json");
    let store = SkillGrantStore::with_path(path);
    let input = serde_json::json!({
        "skill": "ego-browser",
        "grant_subject": user_subject("ego-browser").to_json(),
        "services": ["com.apple.trustd", "com.houyi.test.entitlement"],
    });
    let result = apply_entitlement(&input, Some(&store));
    assert_eq!(
        result["granted"],
        serde_json::json!(["com.houyi.test.entitlement"])
    );
    assert_eq!(
        result["message"],
        "Authorized 1 service(s) for ego-browser."
    );
    assert!(
        store
            .grant_for(&user_subject("ego-browser"))
            .contains(&"com.houyi.test.entitlement".to_string())
    );
    let _ = fs::remove_dir_all(&dir).is_ok();
}

#[test]
fn test_entitlement_persist_failure() {
    let dir = env::temp_dir().join(format!("houyi-entitlement-error-{}", process::id()));
    let _cleanup = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir persistence test");
    let store = SkillGrantStore::with_path(dir.clone());
    let input = serde_json::json!({
        "skill": "ego-browser",
        "grant_subject": user_subject("ego-browser").to_json(),
        "services": ["com.houyi.test.entitlement"],
    });
    let result = apply_entitlement(&input, Some(&store));
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|e| e.contains("persistence failed"))
    );
    assert!(result["granted"].as_array().is_some_and(Vec::is_empty));
    assert!(store.grant_for(&user_subject("ego-browser")).is_empty());
    let _cleanup = fs::remove_dir_all(&dir);
}

#[test]
fn test_entitlement_no_store() {
    let input = serde_json::json!({
        "skill": "x",
        "grant_subject": user_subject("x").to_json(),
        "services": ["a.b.c"],
    });
    let result = apply_entitlement(&input, None);
    assert_eq!(result["error"], "grant store not wired");
}

#[test]
fn test_entitlement_empty_skill_refused() {
    let dir = env::temp_dir().join(format!("houyi-entitlement-empty-{}", process::id()));
    let _ = fs::remove_dir_all(&dir).is_ok();
    fs::create_dir_all(&dir).expect("mkdir empty-skill test");
    let path = dir.join("skill-grants.json");
    let store = SkillGrantStore::with_path(path);
    let input = serde_json::json!({ "skill": "", "services": ["a.b.c"] });
    let result = apply_entitlement(&input, Some(&store));
    assert_eq!(result["error"], "no skill named");
    assert!(
        store.grant_for(&user_subject("unused")).is_empty(),
        "empty-name grant must not be written"
    );
    let _ = fs::remove_dir_all(&dir).is_ok();
}
