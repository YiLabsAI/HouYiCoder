use super::*;
use std::env::temp_dir;
use std::fs;
use std::process::id;

fn create_skill_md(dir: &Path, name: &str, description: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
    )
    .unwrap();
}

#[test]
fn test_discover_finds_skill() {
    let tmp = temp_dir().join(format!("skill-test-{}", id()));
    let skill_dir = tmp.join(".houyicoder").join("skills").join("my-skill");
    create_skill_md(&skill_dir, "my-skill", "Does a thing");

    let found = discover_skills(Some(&tmp), None);
    assert!(found.iter().any(|s| s.name == "my-skill"));

    drop(fs::remove_dir_all(&tmp));
}

#[test]
fn test_discover_skips_pruned_dirs() {
    let tmp = temp_dir().join(format!("skill-prune-{}", id()));
    let git_skill = tmp
        .join(".houyicoder")
        .join("skills")
        .join(".git")
        .join("evil");
    create_skill_md(&git_skill, "evil", "should not load");
    let node_skill = tmp
        .join(".houyicoder")
        .join("skills")
        .join("node_modules")
        .join("evil2");
    create_skill_md(&node_skill, "evil2", "should not load");

    let found = discover_skills(Some(&tmp), None);
    assert!(
        !found.iter().any(|s| s.name == "evil"),
        ".git must be pruned"
    );
    assert!(
        !found.iter().any(|s| s.name == "evil2"),
        "node_modules must be pruned"
    );

    drop(fs::remove_dir_all(&tmp));
}

#[test]
fn test_shadowing_precedence_wins() {
    let tmp = temp_dir().join(format!("skill-shadow-{}", id()));
    let proj = tmp.join(".houyicoder").join("skills").join("shared");
    create_skill_md(&proj, "shared", "from project");
    let found = discover_skills(Some(&tmp), None);
    let shared = found.iter().find(|s| s.name == "shared");
    assert!(shared.is_some());
    assert_eq!(shared.unwrap().description, "from project");

    drop(fs::remove_dir_all(&tmp));
}

#[test]
fn test_walk_finds_nested() {
    let tmp = temp_dir().join(format!("skill-nested-{}", id()));
    let nested = tmp.join("a").join("b").join("c");
    create_skill_md(&nested, "nested", "deep");

    let mut paths = Vec::new();
    walk_for_skill_md(&tmp, &mut paths, 0);
    assert_eq!(paths.len(), 1);

    drop(fs::remove_dir_all(&tmp));
}

#[test]
fn test_walk_depth_limit() {
    let tmp = temp_dir().join(format!("skill-depth-{}", id()));
    let deep = tmp
        .join("a")
        .join("b")
        .join("c")
        .join("d")
        .join("e")
        .join("f")
        .join("g");
    create_skill_md(&deep, "too-deep", "should not be found");

    let mut paths = Vec::new();
    walk_for_skill_md(&tmp, &mut paths, 0);
    assert!(
        paths.is_empty(),
        "deeper than MAX_WALK_DEPTH must not be found"
    );

    drop(fs::remove_dir_all(&tmp));
}

/// The three config-dir families are all scanned: a skill under
/// .claude/skills (ecosystem-compat, zero-migration reuse) and one
/// under .agents/skills (spec interop convention) are both discovered,
/// not just the native config dir.
#[test]
fn test_discover_scans_all_families() {
    let tmp = temp_dir().join(format!("skill-families-{}", id()));
    create_skill_md(
        &tmp.join(".houyicoder").join("skills").join("native-skill"),
        "native-skill",
        "native",
    );
    create_skill_md(
        &tmp.join(".claude").join("skills").join("claude-skill"),
        "claude-skill",
        "claude eco",
    );
    create_skill_md(
        &tmp.join(".agents").join("skills").join("agents-skill"),
        "agents-skill",
        "spec",
    );

    let found = discover_skills(Some(&tmp), None);
    let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"native-skill"),
        "native family scanned: {names:?}"
    );
    assert!(
        names.contains(&"claude-skill"),
        ".claude family scanned: {names:?}"
    );
    assert!(
        names.contains(&"agents-skill"),
        ".agents family scanned: {names:?}"
    );

    drop(fs::remove_dir_all(&tmp));
}
