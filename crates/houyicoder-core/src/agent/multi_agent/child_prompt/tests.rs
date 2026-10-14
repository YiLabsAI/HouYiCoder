//! Tests for this module.

use super::*;
use crate::agent::apply_effort_settings;
use houyicoder_protocol::llm::ModelSettings;
use std::fs;
use std::path::PathBuf;

/// A per-process temp dir so parallel test runs do not collide.
fn scratch_dir(label: &str) -> PathBuf {
    let mut p = temp_dir();
    p.push(format!("child-prompt-test-{}-{}", label, id()));
    fs::create_dir_all(&p).expect("mkdir scratch");
    p
}

#[test]
fn test_env_block_appended_tail() {
    let dir = scratch_dir("tail");
    let p = child_system_prompt("Base instructions.", &dir, "qwen3-coder", false);
    assert!(p.starts_with("Base instructions."));
    assert!(p.ends_with("</env>"), "env block is the tail: {p}");
    assert!(p.contains("Working directory: "));
    assert!(p.contains("Platform: "));
    assert!(
        p.contains("You are powered by the model named qwen3-coder."),
        "{p}"
    );
}

#[test]
fn test_git_repo_detected() {
    let dir = scratch_dir("git-yes");
    fs::create_dir_all(dir.join(".git")).expect("mkdir .git");
    let p = child_system_prompt("base", &dir, "m", false);
    assert!(p.contains("Is directory a git repo: Yes"), "{p}");
}

#[test]
fn test_git_repo_walks_up() {
    let root = scratch_dir("git-walk");
    fs::create_dir_all(root.join(".git")).expect("mkdir .git");
    let sub = root.join("sub");
    fs::create_dir_all(&sub).expect("mkdir sub");
    let p = child_system_prompt("base", &sub, "m", false);
    assert!(p.contains("Is directory a git repo: Yes"), "{p}");
}

#[test]
fn test_plain_dir_not_repo() {
    let dir = scratch_dir("git-no");
    let p = child_system_prompt("base", &dir, "m", false);
    assert!(p.contains("Is directory a git repo: No"), "{p}");
}

/// The project memory rides the system prompt, where the parent carries
/// it too. Injecting it into the child's first user message instead put a
/// whole memory file into the recorded conversation, which the parent's
/// inline view then showed above the task.
#[test]
fn test_prompt_inherits_memory() {
    let dir = scratch_dir("ctx-inherit");
    fs::write(dir.join("AGENTS.md"), "project rules here").expect("write");
    let p = child_system_prompt("base", &dir, "m", false);
    assert!(p.contains("project rules here"), "{p}");
    assert!(p.contains("Today's date is "), "{p}");
    assert!(p.ends_with("</env>"), "env block stays the tail: {p}");
}

/// The omit flag drops the project memory; the date survives, because a
/// child that reads or writes anything time-sensitive still needs it.
#[test]
fn test_prompt_omits_memory() {
    let dir = scratch_dir("ctx-omit");
    fs::write(dir.join("AGENTS.md"), "secret project rules").expect("write");
    let p = child_system_prompt("base", &dir, "m", true);
    assert!(!p.contains("secret project rules"), "{p}");
    assert!(p.contains("Today's date is "), "{p}");
}

#[test]
fn test_prompt_without_memory_file() {
    let dir = scratch_dir("ctx-none");
    let p = child_system_prompt("base", &dir, "m", false);
    assert!(!p.contains("# Project context"), "{p}");
    assert!(p.contains("Today's date is "), "{p}");
}

/// The cost gate: a child's requests carry no extended thinking. On the
/// qwen dialect the lowest tier maps to thinking explicitly disabled.
#[test]
fn test_child_effort_disables_thinking() {
    let mut s = ModelSettings::default();
    apply_effort_settings(&mut s, "qwen3-coder", resolve_child_effort());
    assert_eq!(s.enable_thinking, Some(false));
    assert!(s.thinking_budget.is_none());
}

/// Reasoning dialects have no hard-off; the lowest tier is the closest
/// available setting.
#[test]
fn test_child_effort_reasoning_low() {
    let mut s = ModelSettings::default();
    apply_effort_settings(&mut s, "o3-mini", resolve_child_effort());
    assert_eq!(s.reasoning_effort, Some(EffortLevel::Low));
}

#[test]
fn test_civil_epoch() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(-1), (1969, 12, 31));
}

/// Leap-day boundaries: 2024-02-29 is day 19782 and 2024-03-01 is 19783.
#[test]
fn test_civil_leap_boundaries() {
    assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    assert_eq!(civil_from_days(19_783), (2024, 3, 1));
}

#[test]
fn test_iso_date_format() {
    assert_eq!(iso_date(0), "1970-01-01");
    assert_eq!(iso_date(86_400), "1970-01-02");
}
