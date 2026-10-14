//! Tests for this module.

use super::*;

#[test]
fn test_arguments_full() {
    let body = "echo $ARGUMENTS";
    let result = substitute_args(body, "hello world");
    assert_eq!(result, "echo hello world");
}

#[test]
fn test_arguments_indexed() {
    let body = "first: $0, second: $1";
    let result = substitute_args(body, "foo bar baz");
    assert_eq!(result, "first: foo, second: bar");
}

#[test]
fn test_shorthand_high_to_low() {
    // $12 must not be split into $1 + "2". 0-indexed: parts[12] = "m".
    let body = "val: $12";
    let result = substitute_args(body, "a b c d e f g h i j k l m");
    assert_eq!(result, "val: m");
}

#[test]
fn test_explicit_index() {
    let body = "val: $ARGUMENTS[2]";
    let result = substitute_args(body, "a b c");
    assert_eq!(result, "val: c");
}

#[test]
fn test_no_placeholder_fallback() {
    let body = "just some text";
    let result = substitute_args(body, "extra input");
    assert!(
        result.contains("\n\nARGUMENTS: extra input"),
        "must append the plain fallback (no markup): {result}"
    );
}

#[test]
fn test_placeholder_suppresses_fallback() {
    let body = "args: $ARGUMENTS";
    let result = substitute_args(body, "hello");
    assert!(
        !result.contains("ARGUMENTS: "),
        "must not append fallback: {result}"
    );
    assert_eq!(result, "args: hello");
}

/// An empty args string replaces $ARGUMENTS with "" (not left literal).
/// None means "no args" (caller skips); "" means "empty args provided".
#[test]
fn test_empty_args_replaces_placeholder() {
    let result = substitute_args("echo $ARGUMENTS", "");
    assert_eq!(result, "echo ", "empty args replaces $ARGUMENTS with empty");
    // No $ARGUMENTS token and empty args: no fallback appended.
    let result2 = substitute_args("no tokens", "");
    assert_eq!(result2, "no tokens", "empty args + no token: no fallback");
}

/// Quoted args parse shell-style: "hello world" is one arg, not two.
#[test]
fn test_quoted_arg_kept_intact() {
    let result = substitute_args("val: $1", "foo \"hello world\" baz");
    assert_eq!(result, "val: hello world", "double-quoted arg is one token");
    let result2 = substitute_args("val: $1", "foo 'hello world' baz");
    assert_eq!(
        result2, "val: hello world",
        "single-quoted arg is one token"
    );
}

/// The boundary guard treats letter/underscore as a word char, so $12
/// does not match inside $12abc (left literal, matching the standard).
#[test]
fn test_boundary_guard_word_char() {
    let result = substitute_args("$12abc", "a b c d e f g h i j k l m");
    assert!(
        result.contains("$12abc"),
        "$12 must not match inside $12abc: {result}"
    );
}

#[test]
fn test_empty_args_unchanged() {
    let body = "no tokens here";
    let result = substitute_args(body, "");
    assert_eq!(result, body);
}

#[test]
fn test_variable_skill_dir() {
    let body = "dir is ${HOUYI_SKILL_DIR}";
    let ctx = SubstitutionContext {
        skill_dir: Some(Path::new("/home/user/skills/my-skill")),
        ..Default::default()
    };
    let result = substitute_variables(body, &ctx);
    assert_eq!(result, "dir is /home/user/skills/my-skill");
}

#[test]
fn test_variable_claude_alias() {
    let body = "dir is ${CLAUDE_SKILL_DIR}";
    let ctx = SubstitutionContext {
        skill_dir: Some(Path::new("/home/user/skills/my-skill")),
        ..Default::default()
    };
    let result = substitute_variables(body, &ctx);
    assert_eq!(result, "dir is /home/user/skills/my-skill");
}

#[test]
fn test_variable_session_id() {
    let body = "sid: ${HOUYI_SESSION_ID}";
    let ctx = SubstitutionContext {
        session_id: Some("abc-123"),
        ..Default::default()
    };
    let result = substitute_variables(body, &ctx);
    assert_eq!(result, "sid: abc-123");
}

#[test]
fn test_variable_missing_left_asis() {
    let body = "dir: ${HOUYI_SKILL_DIR}";
    let ctx = SubstitutionContext::default();
    let result = substitute_variables(body, &ctx);
    assert_eq!(result, body);
}

#[test]
fn test_boundary_guard() {
    // $1 must not partially match inside $10. Both should match
    // independently: $10 → parts[10], $1 → parts[1].
    let body = "$1 and $10";
    let result = substitute_args(body, "a b c d e f g h i j k");
    // $10 = parts[10] = "k", $1 = parts[1] = "b". Boundary guard
    // prevents $1 from matching the "1" inside "$10".
    assert_eq!(result, "b and k");
}

use crate::definition::{SkillContext, SkillFamily, SkillProvenance, SkillSource, SpecFields};
use serde_yaml::Mapping;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::PathBuf;
use std::process::id;

/// Write a SKILL.md + the given resource files into a temp skill dir,
/// return a definition pointing at it with the given source.
fn def_with(source: SkillSource, files: &[(&str, &str)]) -> (PathBuf, SkillDefinition) {
    let dir = temp_dir().join(format!("skill-invoke-{}-{}", source_as_str(&source), id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("mkdir");
    write(
        dir.join("SKILL.md"),
        "---\nname: x\ndescription: d\n---\nbody\n",
    )
    .expect("write SKILL.md");
    for (rel, content) in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            create_dir_all(parent).expect("mkdir");
        }
        write(p, content).expect("write");
    }
    let def = SkillDefinition {
        name: "x".into(),
        display_name: None,
        description: "d".into(),
        when_to_use: None,
        allowed_tools: Vec::new(),
        allowed_mach_services: Vec::new(),
        allow_app_launch: false,
        argument_hint: None,
        version: None,
        model: None,
        effort: None,
        disable_model_invocation: false,
        user_invocable: true,
        context: SkillContext::Inline,
        paths: Vec::new(),
        source,
        body_path: dir.join("SKILL.md"),
        skill_dir: dir.clone(),
        hooks_raw: None,
        spec_fields: SpecFields::default(),
        unknown_fields: Mapping::new(),
    };
    (dir, def)
}

fn source_as_str(source: &SkillSource) -> &'static str {
    match source.family {
        SkillFamily::Houyi => "houyi",
        SkillFamily::ClaudeEco => "eco",
        SkillFamily::Agents => "agents",
        SkillFamily::Mcp => "mcp",
    }
}

fn project_source(family: SkillFamily) -> SkillSource {
    SkillSource::new(
        family,
        SkillProvenance::Project {
            root: Path::new("/tmp").to_path_buf(),
        },
    )
}

/// An eligible source (project) gets the resource manifest appended
/// to the prepared body: the scripts and reference files appear by
/// path so the model can address them.
#[test]
fn test_manifest_appended_for_eligible() {
    let (dir, def) = def_with(
        project_source(SkillFamily::Houyi),
        &[
            ("scripts/deploy.py", "print('hi')\n"),
            ("reference.md", "see\n"),
        ],
    );
    let ctx = SubstitutionContext {
        skill_dir: Some(&def.skill_dir),
        ..Default::default()
    };
    let body = prepare_body(&def, None, &ctx).expect("prepared");
    assert!(
        body.contains("Base directory for this skill"),
        "header kept"
    );
    assert!(
        body.contains("Resources in this skill's directory"),
        "manifest appended: {body}"
    );
    assert!(body.contains("script: scripts/deploy.py"), "{body}");
    assert!(body.contains("reference: reference.md"), "{body}");
    drop(remove_dir_all(&dir));
}

/// A non-eligible source (ecosystem-compat path) gets no manifest: a
/// crafted file set in a shared repository must not be surfaced.
#[test]
fn test_manifest_skipped_for_eco() {
    let (dir, def) = def_with(
        project_source(SkillFamily::ClaudeEco),
        &[("scripts/evil.py", "rm -rf /\n")],
    );
    let ctx = SubstitutionContext {
        skill_dir: Some(&def.skill_dir),
        ..Default::default()
    };
    let body = prepare_body(&def, None, &ctx).expect("prepared");
    assert!(
        !body.contains("Resources in this skill's directory"),
        "no manifest for eco source: {body}"
    );
    assert!(
        !body.contains("evil.py"),
        "eco resource file not surfaced: {body}"
    );
    drop(remove_dir_all(&dir));
}
