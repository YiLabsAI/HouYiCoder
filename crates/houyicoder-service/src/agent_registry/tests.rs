//! Tests for this module.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use houyicoder_core::agent::multi_agent::registry::ResolveCtx;

use super::build_agent_registry;
use std::env::temp_dir;
use std::process::id;

#[test]
fn test_registry_resolves_builtins() {
    let reg = build_agent_registry(None, None);
    for ty in ["general-purpose", "explore", "plan", "verify", "code-guide"] {
        let def = reg
            .resolve(ty, &ResolveCtx::default())
            .unwrap_or_else(|e| panic!("resolve {ty} failed: {e:?}"));
        assert_eq!(def.subagent_type, ty);
    }
    assert_eq!(reg.list().len(), 5);
}

/// A user-layer file overrides the built-in of the same type.
#[test]
fn test_user_overrides_builtin() {
    let dir = unique_temp_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("explore.md"),
        "---\nname: explore\ndescription: user-customized\n---\nbody",
    )
    .unwrap();
    let reg = build_agent_registry(Some(&dir), None);
    let def = reg.resolve("explore", &ResolveCtx::default()).unwrap();
    assert_eq!(def.when_to_use, "user-customized");
    fs::remove_dir_all(&dir).ok();
}

/// Project overrides user for the same type; distinct types coexist.
#[test]
fn test_project_overrides_user() {
    let user_dir = unique_temp_dir();
    let project_dir = unique_temp_dir();
    fs::create_dir_all(&user_dir).unwrap();
    fs::create_dir_all(&project_dir).unwrap();
    fs::write(
        user_dir.join("shared.md"),
        "---\nname: shared\ndescription: user\n---\nu",
    )
    .unwrap();
    fs::write(
        project_dir.join("shared.md"),
        "---\nname: shared\ndescription: project\n---\np",
    )
    .unwrap();
    let reg = build_agent_registry(Some(&user_dir), Some(&project_dir));
    let def = reg.resolve("shared", &ResolveCtx::default()).unwrap();
    assert_eq!(def.when_to_use, "project");
    // Built-ins survive alongside layered definitions.
    assert!(
        reg.resolve("general-purpose", &ResolveCtx::default())
            .is_ok()
    );
    fs::remove_dir_all(&user_dir).ok();
    fs::remove_dir_all(&project_dir).ok();
}

fn unique_temp_dir() -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    temp_dir().join(format!("houyi-agentreg-{}-{}", id(), n))
}
