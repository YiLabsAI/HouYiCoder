//! Tests for this module.

use super::*;
use crate::definition::{SkillFamily, SkillProvenance};
use serde_yaml::Mapping;
use serde_yaml::Value;

fn user_source() -> SkillSource {
    SkillSource::new(SkillFamily::Houyi, SkillProvenance::UserHome)
}

fn project_source() -> SkillSource {
    SkillSource::new(
        SkillFamily::Houyi,
        SkillProvenance::Project {
            root: Path::new("/tmp").to_path_buf(),
        },
    )
}

#[test]
fn test_parse_minimal_skill() {
    let text = "---\nname: my-skill\ndescription: Does a thing\n---\nbody text";
    let def = parse_skill(
        text,
        "my-skill",
        Path::new("/tmp/my-skill"),
        Path::new("/tmp/my-skill/SKILL.md"),
        user_source(),
    )
    .expect("parse");
    assert_eq!(def.name, "my-skill");
    assert_eq!(def.description, "Does a thing");
    assert!(def.user_invocable);
}

#[test]
fn test_parse_allow_app_launch() {
    let text = "---\nname: launcher\ndescription: test\nallow-app-launch: true\n---\nbody";
    let def = parse_skill(
        text,
        "launcher",
        Path::new("/tmp/launcher"),
        Path::new("/tmp/launcher/SKILL.md"),
        user_source(),
    )
    .expect("parse");
    assert!(
        def.allow_app_launch,
        "allow-app-launch frontmatter reaches the definition"
    );
}

#[test]
fn test_parse_app_launch_absent() {
    let text = "---\nname: plain\ndescription: test\n---\nbody";
    let def = parse_skill(
        text,
        "plain",
        Path::new("/tmp/plain"),
        Path::new("/tmp/plain/SKILL.md"),
        user_source(),
    )
    .expect("parse");
    assert!(
        !def.allow_app_launch,
        "allow-app-launch defaults to false when absent"
    );
}

#[test]
fn test_description_from_body() {
    let text = "---\nname: no-desc\n---\nThis is the first line of body.";
    let def = parse_skill(
        text,
        "no-desc",
        Path::new("/tmp/no-desc"),
        Path::new("/tmp/no-desc/SKILL.md"),
        project_source(),
    )
    .expect("parse");
    assert_eq!(def.description, "This is the first line of body.");
}

#[test]
fn test_missing_description_skips() {
    let text = "---\nname: empty\n---\n";
    assert!(
        parse_skill(
            text,
            "empty",
            Path::new("/tmp/empty"),
            Path::new("/tmp/empty/SKILL.md"),
            project_source()
        )
        .is_err()
    );
}

#[test]
fn test_unknown_fields_pass_through() {
    let text = "---\nname: test\ndescription: test\ncustom_field: value\n---\nbody";
    let def = parse_skill(
        text,
        "test",
        Path::new("/tmp/test"),
        Path::new("/tmp/test/SKILL.md"),
        user_source(),
    )
    .expect("parse");
    assert!(
        def.unknown_fields
            .contains_key(Value::String("custom_field".into()))
    );
}

#[test]
fn test_name_mismatch_warns() {
    let text = "---\nname: DifferentName\ndescription: test\n---\nbody";
    let def = parse_skill(
        text,
        "dir-name",
        Path::new("/tmp/dir-name"),
        Path::new("/tmp/dir-name/SKILL.md"),
        user_source(),
    )
    .expect("parse");
    // Directory name is identity; frontmatter name is display-only.
    assert_eq!(def.name, "dir-name", "directory name is the identity");
    assert_eq!(
        def.display_name.as_deref(),
        Some("DifferentName"),
        "frontmatter name is display-only"
    );
}

#[test]
fn test_fork_context() {
    let text = "---\nname: forked\ndescription: test\ncontext: fork\nagent: reviewer\n---\nbody";
    let def = parse_skill(
        text,
        "forked",
        Path::new("/tmp/forked"),
        Path::new("/tmp/forked/SKILL.md"),
        user_source(),
    )
    .expect("parse");
    assert_eq!(def.context, SkillContext::Fork("reviewer".into()));
}

#[test]
fn test_bad_yaml_recovers() {
    let text = "---\nname: broken\ndescription: test\nbad: value: with: colons\n---\nbody";
    assert!(
        parse_skill(
            text,
            "broken",
            Path::new("/tmp/broken"),
            Path::new("/tmp/broken/SKILL.md"),
            user_source()
        )
        .is_ok()
    );
}

#[test]
fn test_name_validation_warns() {
    let text = "---\nname: Bad_Name\ndescription: test\n---\nbody";
    assert!(
        parse_skill(
            text,
            "Bad_Name",
            Path::new("/tmp/Bad_Name"),
            Path::new("/tmp/Bad_Name/SKILL.md"),
            user_source()
        )
        .is_ok()
    );
}

/// field_bool accepts both YAML bool and string "true"/"false", so the
/// YAML-fallback recovery path (which stores everything as String) does
/// not lose bool fields. Without this, disable-model-invocation would
/// fail-open (false) on a malformed-YAML recovery.
#[test]
fn test_field_bool_string_form() {
    let mut map = Mapping::new();
    map.insert(Value::String("k".into()), Value::String("true".into()));
    assert_eq!(field_bool(&map, "k"), Some(true), "String(true) -> true");

    map.insert(Value::String("k".into()), Value::String("false".into()));
    assert_eq!(field_bool(&map, "k"), Some(false), "String(false) -> false");

    map.insert(Value::String("k".into()), Value::Bool(true));
    assert_eq!(field_bool(&map, "k"), Some(true), "Bool(true) -> true");
}

/// A malformed-YAML recovery preserves disable-model-invocation. The
/// tab in indentation breaks serde_yaml; the scalar recovery stores
/// "true" as a string; field_bool must read it. Under the old code this
/// returned false (fail-open: the model could invoke a disabled skill).
#[test]
fn test_malformed_yaml_preserves_disable() {
    let text = "---\n\tdisable-model-invocation: true\ndescription: test\nname: test\n---\nbody";
    let def = parse_skill(
        text,
        "test",
        Path::new("/tmp/test"),
        Path::new("/tmp/test/SKILL.md"),
        user_source(),
    )
    .expect("parse with recovery");
    assert!(
        def.disable_model_invocation,
        "disable-model-invocation preserved through YAML recovery"
    );
}

fn paths_map(value: Value) -> Mapping {
    let mut m = Mapping::new();
    m.insert(Value::String("paths".into()), value);
    m
}

#[test]
fn test_paths_comma_split() {
    let map = paths_map(Value::String("src, docs".into()));
    assert_eq!(
        parse_skill_paths(&map),
        vec!["src".to_string(), "docs".to_string()]
    );
}

#[test]
fn test_paths_yaml_list() {
    let seq = Value::Sequence(vec![
        Value::String("src".into()),
        Value::String("docs".into()),
    ]);
    let map = paths_map(seq);
    assert_eq!(
        parse_skill_paths(&map),
        vec!["src".to_string(), "docs".to_string()]
    );
}

#[test]
fn test_paths_brace_single() {
    let map = paths_map(Value::String("src/*.{ts,tsx}".into()));
    assert_eq!(
        parse_skill_paths(&map),
        vec!["src/*.ts".to_string(), "src/*.tsx".to_string()]
    );
}

#[test]
fn test_paths_brace_cross() {
    // {a,b}/{c,d} expands to the four crosses, left-to-right.
    let map = paths_map(Value::String("{a,b}/{c,d}".into()));
    assert_eq!(
        parse_skill_paths(&map),
        vec![
            "a/c".to_string(),
            "a/d".to_string(),
            "b/c".to_string(),
            "b/d".to_string(),
        ]
    );
}

#[test]
fn test_paths_strip_suffix() {
    // A bare directory name matches itself + descendants under
    // gitignore semantics, so the trailing /** is stripped at parse.
    let map = paths_map(Value::String("src/**".into()));
    assert_eq!(parse_skill_paths(&map), vec!["src".to_string()]);
}

#[test]
fn test_paths_allstar_unconditional() {
    // A lone ** matches everything, so the skill is unconditional
    // (empty = always visible).
    let map = paths_map(Value::String("**".into()));
    assert!(parse_skill_paths(&map).is_empty());
    let seq = Value::Sequence(vec![Value::String("**".into()), Value::String("**".into())]);
    let map = paths_map(seq);
    assert!(parse_skill_paths(&map).is_empty());
}

#[test]
fn test_paths_missing_unconditional() {
    let map = Mapping::new();
    assert!(parse_skill_paths(&map).is_empty());
}

/// A malformed paths value (a mapping where a string or list is
/// expected) fails open to empty — the skill becomes unconditional
/// (always visible). This is intentional: skill visibility defaults to
/// on, and a broken pattern widening to always-visible is the safe
/// direction. Pinning it here stops a future change from silently
/// flipping to fail-closed.
#[test]
fn test_paths_malformed_open() {
    let nested = Value::Mapping(Mapping::new());
    let map = paths_map(nested);
    assert!(
        parse_skill_paths(&map).is_empty(),
        "malformed paths value must fail open to unconditional"
    );
}

/// An unmatched brace (no closing brace) is treated as a literal
/// character — no expansion, the pattern survives as-is. Pins the
/// documented behavior so a refactor cannot silently change it.
#[test]
fn test_paths_unmatched_brace() {
    let map = paths_map(Value::String("foo{bar".into()));
    assert_eq!(parse_skill_paths(&map), vec!["foo{bar".to_string()]);
}
