//! Tests for this module.

use super::*;
use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::skill::SkillRegistry;
use houyicoder_api::trust::TrustState;
use houyicoder_core::agent::ConditionalSkillActivator;
use houyicoder_core::agent::{ConditionalActivation, HookRegistry, SkillHookRegistrar};
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::process::id;
use std::sync::{Arc, RwLock};

fn write_skill(dir: &Path, name: &str) {
    let skill_dir = dir.join(".houyicoder").join("skills").join(name);
    create_dir_all(&skill_dir).unwrap();
    write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name}\n---\nbody\n"),
    )
    .unwrap();
}

fn deps_with(tmp: &Path) -> ReloadDeps {
    let registry = Arc::new(SkillRegistryImpl::discover_with_home(Some(tmp), None));
    let activator: Arc<dyn ConditionalSkillActivator> = Arc::new(ConditionalActivation::new(
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
    let tmp = temp_dir().join(format!("skill-reload-deps-{}", id()));
    drop(remove_dir_all(&tmp));
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
    drop(remove_dir_all(&tmp));
}

#[test]
fn test_start_no_roots_none() {
    let tmp = temp_dir().join(format!("skill-reload-noroots-{}", id()));
    drop(remove_dir_all(&tmp));
    create_dir_all(&tmp).unwrap();
    let registry = Arc::new(SkillRegistryImpl::discover_with_home(Some(&tmp), None));
    let activator: Arc<dyn ConditionalSkillActivator> = Arc::new(ConditionalActivation::new(
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
    drop(remove_dir_all(&tmp));
}

#[test]
fn test_drop_stops_watcher() {
    let tmp = temp_dir().join(format!("skill-reload-drop-{}", id()));
    drop(remove_dir_all(&tmp));
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
    drop(remove_dir_all(&tmp));
}

#[test]
fn test_is_skills_dir() {
    let tmp = temp_dir().join(format!("skill-isd-{}", id()));
    drop(remove_dir_all(&tmp));
    create_dir_all(tmp.join("skills")).unwrap();
    create_dir_all(tmp.join("other")).unwrap();
    write(tmp.join("SKILL.md"), "x").unwrap();
    assert!(is_skills_dir(&tmp.join("skills")));
    assert!(!is_skills_dir(&tmp.join("other")));
    assert!(
        !is_skills_dir(&tmp.join("SKILL.md")),
        "a file is not a skills dir"
    );
    drop(remove_dir_all(&tmp));
}

#[test]
fn test_file_size() {
    let tmp = temp_dir().join(format!("skill-fs-{}", id()));
    drop(remove_dir_all(&tmp));
    create_dir_all(&tmp).unwrap();
    write(tmp.join("f"), "hello").unwrap();
    assert_eq!(file_size(&tmp.join("f")), Some(5));
    assert_eq!(file_size(&tmp.join("missing")), None);
    drop(remove_dir_all(&tmp));
}
