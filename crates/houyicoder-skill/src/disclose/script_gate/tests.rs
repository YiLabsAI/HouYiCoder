use super::*;
use crate::definition::{SkillFamily, SkillProvenance, SkillSource};
use std::path::Path;

fn managed_source() -> SkillSource {
    SkillSource::new(SkillFamily::Houyi, SkillProvenance::Managed)
}

fn project_source() -> SkillSource {
    SkillSource::new(
        SkillFamily::Houyi,
        SkillProvenance::Project {
            root: Path::new("/srv").to_path_buf(),
        },
    )
}

fn skills() -> Vec<(String, SkillSource, &'static Path)> {
    vec![
        (
            "deploy".into(),
            project_source(),
            Path::new("/srv/skills/deploy"),
        ),
        // deployment prefixes deploy — a collision guard for the path
        // boundary check (a shorter name must not match a longer one).
        (
            "deployment".into(),
            managed_source(),
            Path::new("/srv/skills/deployment"),
        ),
        (
            "tool".into(),
            managed_source(),
            Path::new("/etc/houyicoder/skills/tool"),
        ),
    ]
}

#[test]
fn test_detects_script_in_scripts() {
    let refs = detect_skill_scripts(
        "python /srv/skills/deploy/scripts/deploy.py --arg",
        &skills(),
    );
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].skill_name, "deploy");
    assert_eq!(refs[0].script_rel_path, "scripts/deploy.py");
    assert_eq!(refs[0].source, project_source());
}

#[test]
fn test_detects_script_by_ext() {
    let refs = detect_skill_scripts("bash /srv/skills/deploy/run.sh", &skills());
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].script_rel_path, "run.sh");
}

#[test]
fn test_skips_reference_read() {
    // A markdown read is not an execution; the resource fence allows
    // it and the gate is for running a script.
    let refs = detect_skill_scripts("cat /srv/skills/deploy/reference.md", &skills());
    assert!(refs.is_empty(), "reference read not flagged: {refs:?}");
}

#[test]
fn test_skips_dir_listing() {
    let refs = detect_skill_scripts("ls /srv/skills/deploy", &skills());
    assert!(refs.is_empty(), "bare dir listing not flagged: {refs:?}");
}

#[test]
fn test_skips_unrelated_command() {
    let refs = detect_skill_scripts("echo hello && ls /tmp", &skills());
    assert!(refs.is_empty(), "no skill dir referenced: {refs:?}");
}

#[test]
fn test_detects_multiple_skills() {
    let refs = detect_skill_scripts(
        "python /srv/skills/deploy/scripts/x.py && bash /etc/houyicoder/skills/tool/scripts/y.sh",
        &skills(),
    );
    assert_eq!(refs.len(), 2, "both skill scripts detected: {refs:?}");
}

#[test]
fn test_quoted_path_detected() {
    let refs = detect_skill_scripts("python \"/srv/skills/deploy/scripts/deploy.py\"", &skills());
    assert_eq!(refs.len(), 1, "quoted script path detected: {refs:?}");
    assert_eq!(refs[0].script_rel_path, "scripts/deploy.py");
}

#[test]
fn test_carries_source() {
    // The managed skill's script carries source Managed so the gate can
    // always-allow; the project skill's carries Project so it asks.
    let refs = detect_skill_scripts("python /etc/houyicoder/skills/tool/scripts/y.py", &skills());
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].source, managed_source());
}

/// A shorter skill name must not match a longer one: a command running
/// deployment's script attributes to deployment, not to the deploy
/// prefix. Without the path-boundary check, deploy would match first,
/// mis-attribute the source, and let a project script skip the Ask.
#[test]
fn test_prefix_collision_attrs_correctly() {
    let refs = detect_skill_scripts("python /srv/skills/deployment/scripts/run.py", &skills());
    assert_eq!(refs.len(), 1, "one skill matched: {refs:?}");
    assert_eq!(
        refs[0].skill_name, "deployment",
        "the longer name wins, not the deploy prefix: {refs:?}"
    );
    assert_eq!(refs[0].script_rel_path, "scripts/run.py");
}

/// A command running two scripts from the same skill yields two refs —
/// the gate is per-script, not per-skill.
#[test]
fn test_same_skill_two_scripts() {
    let refs = detect_skill_scripts(
        "python /srv/skills/deploy/scripts/a.py && python /srv/skills/deploy/scripts/b.py",
        &skills(),
    );
    assert_eq!(refs.len(), 2, "two scripts, two refs: {refs:?}");
    let paths: Vec<_> = refs.iter().map(|r| r.script_rel_path.clone()).collect();
    assert!(paths.contains(&"scripts/a.py".to_string()), "{paths:?}");
    assert!(paths.contains(&"scripts/b.py".to_string()), "{paths:?}");
}

/// A trailing-slash directory (python on a directory) is not a script
/// run — it errors at runtime, and flagging it would be a spurious Ask.
#[test]
fn test_skips_trailing_slash_dir() {
    let refs = detect_skill_scripts("python /srv/skills/deploy/scripts/", &skills());
    assert!(refs.is_empty(), "trailing-slash dir not flagged: {refs:?}");
}

/// A bare scripts directory path with no file under it is not a script
/// run. classify treats only a file under scripts/ as a Script, not the
/// directory itself, so an ls or cd into the scripts dir does not
/// raise a per-script Ask on a wrong path.
#[test]
fn test_bare_scripts_dir_skipped() {
    let refs = detect_skill_scripts("ls /srv/skills/deploy/scripts", &skills());
    assert!(refs.is_empty(), "bare scripts dir not flagged: {refs:?}");
}

/// A cd into the skill dir then a relative script run evades the gate:
/// the cd match is dropped because the dir is followed by a space, not
/// a slash, and the relative path carries no canonical directory string.
/// Pins the honest coverage boundary — the gate catches the
/// canonical-path form, not every form that resolves to a skill script.
#[test]
fn test_cd_relative_evades() {
    let refs = detect_skill_scripts(
        "cd /srv/skills/deploy && python scripts/deploy.py",
        &skills(),
    );
    assert!(
        refs.is_empty(),
        "cd+relative evades the literal match: {refs:?}"
    );
}

/// An unexpanded environment variable evades the gate for the same
/// reason: the literal canonical path is not present. The shell expands
/// the variable at exec time, after the gate has run.
#[test]
fn test_unexpanded_var_evades() {
    let refs = detect_skill_scripts("python $HOUYI_SKILL_DIR/scripts/deploy.py", &skills());
    assert!(
        refs.is_empty(),
        "unexpanded var evades the literal match: {refs:?}"
    );
}

/// A relative path from a different cwd evades for the same reason:
/// the canonical skill path is not in the command, so there is nothing
/// to match.
#[test]
fn test_relative_from_cwd_evades() {
    let refs = detect_skill_scripts("python skills/deploy/scripts/deploy.py", &skills());
    assert!(
        refs.is_empty(),
        "relative path from another cwd evades: {refs:?}"
    );
}
