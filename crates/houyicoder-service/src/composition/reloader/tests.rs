//! Tests for this module.

use super::*;
use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::skill::SkillRegistry;
use houyicoder_api::trust::TrustState;
use houyicoder_core::agent::{ConditionalActivation, HookRegistry, SkillHookRegistrar};
use std::sync::{Arc, RwLock};

fn write_skill(dir: &Path, name: &str) {
    let skill_dir = dir.join(".houyicoder").join("skills").join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name}\n---\nbody\n"),
    )
    .unwrap();
}

fn deps_with(tmp: &Path) -> ReloadDeps {
    let registry = Arc::new(SkillRegistryImpl::discover_with_home(Some(tmp), None));
    let activator: Arc<dyn houyicoder_core::agent::ConditionalSkillActivator> =
        Arc::new(ConditionalActivation::new(
            Arc::clone(&registry) as Arc<dyn houyicoder_api::skill::SkillRegistry>,
            tmp.to_path_buf(),
        ));
    let hook_reg = Arc::new(HookRegistry::new());
    let trust = Arc::new(RwLock::new(TrustState::Trusted));
    let registrar = Arc::new(SkillHookRegistrar::new(
        hook_reg,
        trust,
        Arc::new(StdProcessLauncher::new()),
    ));
    ReloadDeps {
        registry,
        activator,
        registrar,
        cwd: Some(tmp.to_path_buf()),
        home: None,
    }
}

#[test]
fn test_reload_picks_new_skill() {
    let tmp = std::env::temp_dir().join(format!("skill-reload-deps-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&tmp));
    write_skill(&tmp, "alpha");
    let deps = deps_with(&tmp);
    assert!(deps.registry.find("alpha").is_some());
    assert!(deps.registry.find("beta").is_none());
    write_skill(&tmp, "beta");
    deps.reload();
    assert!(
        deps.registry.find("beta").is_some(),
        "beta picked up after reload"
    );
    assert!(
        deps.registry.find("alpha").is_some(),
        "alpha survives reload"
    );
    drop(std::fs::remove_dir_all(&tmp));
}

#[test]
fn test_start_no_roots_none() {
    let tmp = std::env::temp_dir().join(format!("skill-reload-noroots-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&tmp));
    std::fs::create_dir_all(&tmp).unwrap();
    let registry = Arc::new(SkillRegistryImpl::discover_with_home(Some(&tmp), None));
    let activator: Arc<dyn houyicoder_core::agent::ConditionalSkillActivator> =
        Arc::new(ConditionalActivation::new(
            Arc::clone(&registry) as Arc<dyn SkillRegistry>,
            tmp.to_path_buf(),
        ));
    let hook_reg = Arc::new(HookRegistry::new());
    let trust = Arc::new(RwLock::new(TrustState::Trusted));
    let registrar = Arc::new(SkillHookRegistrar::new(
        hook_reg,
        trust,
        Arc::new(StdProcessLauncher::new()),
    ));
    let guard = SkillReloader::start(registry, activator, registrar, Some(tmp.clone()), None);
    assert!(guard.is_none(), "no roots -> None (no thread spawned)");
    drop(std::fs::remove_dir_all(&tmp));
}

#[test]
fn test_drop_stops_watcher() {
    let tmp = std::env::temp_dir().join(format!("skill-reload-drop-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&tmp));
    write_skill(&tmp, "alpha");
    let deps = deps_with(&tmp);
    let guard = SkillReloader::start(
        deps.registry,
        deps.activator,
        deps.registrar,
        deps.cwd,
        deps.home,
    )
    .expect("watch roots exist");
    let started = Instant::now();
    drop(guard);
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(std::fs::remove_dir_all(&tmp));
}

#[test]
fn test_is_skills_dir() {
    let tmp = std::env::temp_dir().join(format!("skill-isd-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&tmp));
    std::fs::create_dir_all(tmp.join("skills")).unwrap();
    std::fs::create_dir_all(tmp.join("other")).unwrap();
    std::fs::write(tmp.join("SKILL.md"), "x").unwrap();
    assert!(is_skills_dir(&tmp.join("skills")));
    assert!(!is_skills_dir(&tmp.join("other")));
    assert!(
        !is_skills_dir(&tmp.join("SKILL.md")),
        "a file is not a skills dir"
    );
    drop(std::fs::remove_dir_all(&tmp));
}

#[test]
fn test_file_size() {
    let tmp = std::env::temp_dir().join(format!("skill-fs-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&tmp));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("f"), "hello").unwrap();
    assert_eq!(file_size(&tmp.join("f")), Some(5));
    assert_eq!(file_size(&tmp.join("missing")), None);
    drop(std::fs::remove_dir_all(&tmp));
}
