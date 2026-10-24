//! System-prompt assembly tests: project memory file discovery,
//! merge order, local overlay, and section layout.

use super::*;
use crate::agent::context::Tokenizer;
use std::env::temp_dir;
use std::fs;
use std::process::id;

/// A per-process temp dir so parallel test runs do not collide.
fn scratch_dir(label: &str) -> PathBuf {
    let mut p = temp_dir();
    p.push(format!("prompt-test-{}-{}", label, id()));
    fs::create_dir_all(&p).expect("mkdir scratch");
    p
}

#[test]
fn test_no_memory_file_identity() {
    let dir = scratch_dir("empty");
    let p = SystemPrompt::build(&dir);
    assert!(!p.text.is_empty(), "system prompt must be non-empty");
    assert!(
        p.memory_blocks.is_empty(),
        "scratch dir has no memory file to report"
    );
    assert!(p.text.contains("agent"));
    assert!(
        !p.text.contains("# Project context"),
        "no project context section when no memory file"
    );
    assert!(p.text.contains("# Efficiency"));
    assert!(p.text.contains("# Tool docs"));
}

#[test]
fn test_agent_directory_injected() {
    let dir = scratch_dir("empty");
    let section = "## Available agents\n\n- explore: fast search";
    let p =
        SystemPrompt::build_with_memory_index(&dir, None, Some(section), &FrozenPrompt::read(&dir));
    assert!(p.text.contains("- explore: fast search"));
    assert!(
        p.memory_blocks.is_empty(),
        "the agent directory is prompt framing, not a memory block"
    );
}

#[test]
fn test_agents_md_injected() {
    let dir = scratch_dir("agents");
    fs::write(dir.join("AGENTS.md"), "# My Project\n\nRules go here.").expect("write");
    let p = SystemPrompt::build(&dir);
    assert_eq!(
        p.memory_blocks.len(),
        1,
        "AGENTS.md must be reported as one memory block"
    );
    assert_eq!(p.memory_blocks[0].label, "AGENTS.md");
    assert!(p.text.contains("My Project"), "content must be injected");
    assert!(p.text.contains("Rules go here."));
    assert!(p.text.contains("# Project context"));
}

#[test]
fn test_falls_back_to_md() {
    let dir = scratch_dir("claude");
    fs::write(dir.join("CLAUDE.md"), "# Fallback rules").expect("write");
    let p = SystemPrompt::build(&dir);
    assert_eq!(
        p.memory_blocks.len(),
        1,
        "CLAUDE.md fallback must be reported"
    );
    assert!(p.text.contains("Fallback rules"));
}

#[test]
fn test_agents_preferred_over_md() {
    let dir = scratch_dir("both");
    fs::write(dir.join("AGENTS.md"), "agents rules").expect("write");
    fs::write(dir.join("CLAUDE.md"), "claude rules").expect("write");
    let found = find_memory_file_path(&dir).expect("found");
    assert!(
        found.ends_with("AGENTS.md"),
        "AGENTS.md preferred over CLAUDE.md"
    );
}

/// The primary name loads first, and a second established spelling in the
/// same directory loads alongside it rather than being shadowed.
#[test]
fn test_primary_name_loads_first() {
    let dir = scratch_dir("agents-md");
    fs::write(dir.join("AGENTS.md"), "agents rules").expect("write");
    fs::write(dir.join("CLAUDE.md"), "claude rules").expect("write");
    let found = find_memory_file_path(&dir).expect("found");
    assert!(
        found.ends_with("AGENTS.md"),
        "AGENTS.md preferred over CLAUDE.md, got {found:?}"
    );
    let p = SystemPrompt::build(&dir);
    assert!(
        p.text.contains("agents rules"),
        "AGENTS.md content injected"
    );
    assert!(
        p.text.contains("claude rules"),
        "CLAUDE.md content co-injected, not shadowed"
    );
    let primary_pos = p.text.find("agents rules").expect("primary text");
    let second_pos = p.text.find("claude rules").expect("second text");
    assert!(
        primary_pos < second_pos,
        "the primary name's content precedes the co-loaded file"
    );
}

/// A case-insensitive filesystem satisfies both spellings of the same name
/// with one file, so a lone CLAUDE.md must load exactly once — the candidate
/// list dedupes by canonical path before joining.
#[test]
fn test_case_fold_loads_once() {
    let dir = scratch_dir("claude-both-case");
    fs::write(dir.join("CLAUDE.md"), "folded rules").expect("write");
    let p = SystemPrompt::build(&dir);
    assert!(
        p.text.matches("folded rules").count() == 1,
        "one file loads once even when the name matches two candidates:\n{}",
        p.text
    );
    assert!(
        p.text
            .to_ascii_lowercase()
            .contains("from the claude.md memory file"),
        "the header names the one file it loaded:\n{}",
        p.text
    );
}

/// The local-private overlay is merged onto the project memory file so
/// personal or machine-specific overrides layer on top of the shared,
/// git-tracked memory.
#[test]
fn test_local_overlay_merged() {
    let dir = scratch_dir("local");
    fs::write(dir.join("AGENTS.md"), "shared rules").expect("write");
    fs::write(dir.join("AGENTS.local.md"), "personal overrides").expect("write");
    let p = SystemPrompt::build(&dir);
    assert!(!p.memory_blocks.is_empty());
    assert!(p.text.contains("shared rules"), "shared memory injected");
    assert!(
        p.text.contains("personal overrides"),
        "local overlay merged onto the shared memory"
    );
}

/// The overlay follows the primary file's spelling: a project that keeps its
/// rules in CLAUDE.md reads CLAUDE.local.md beside it.
#[test]
fn test_overlay_follows_spelling() {
    let dir = scratch_dir("local-claude");
    fs::write(dir.join("CLAUDE.md"), "claude rules").expect("write");
    fs::write(dir.join("CLAUDE.local.md"), "claude personal").expect("write");
    let p = SystemPrompt::build(&dir);
    assert!(p.text.contains("claude rules"));
    assert!(
        p.text.contains("claude personal"),
        "the second spelling's overlay is read too"
    );
}

#[test]
fn test_walk_up_finds_parent() {
    let root = scratch_dir("root");
    fs::write(root.join("AGENTS.md"), "parent rules").expect("write");
    let child = root.join("sub").join("deep");
    fs::create_dir_all(&child).expect("mkdir child");
    let p = SystemPrompt::build(&child);
    assert!(
        !p.memory_blocks.is_empty(),
        "walk-up must reach the parent file"
    );
    assert!(p.text.contains("parent rules"));
}

#[test]
fn test_byte_stable_across_turns() {
    let dir = scratch_dir("stable");
    fs::write(dir.join("AGENTS.md"), "stable content").expect("write");
    let a = SystemPrompt::build(&dir);
    let b = SystemPrompt::build(&dir);
    assert_eq!(a.text, b.text, "same inputs must produce identical bytes");
    assert_eq!(a.memory_blocks, b.memory_blocks);
}

#[test]
fn test_token_count_positive() {
    let dir = scratch_dir("tok");
    fs::write(dir.join("AGENTS.md"), "some content for token count").expect("write");
    let p = SystemPrompt::build(&dir);
    let t = Tokenizer::new();
    assert!(t.count(&p.text) > 0, "built prompt must tokenize to > 0");
}

#[test]
fn test_tone_guides_style() {
    // Tone section: no emoji, file_path:line references, period before
    // tool calls (not colon).
    let dir = scratch_dir("tone");
    fs::write(dir.join("AGENTS.md"), "x").expect("write");
    let p = SystemPrompt::build(&dir);
    assert!(p.text.contains("# Tone and style"), "{}", p.text);
    assert!(p.text.contains("Only use emojis"), "{}", p.text);
    assert!(p.text.contains("file_path:line_number"), "{}", p.text);
    assert!(p.text.contains("period"), "{}", p.text);
}

#[test]
fn test_system_guides_framework() {
    // The system section must name the framework rules: denied tools are
    // not re-attempted, prompt-injection flagging, auto-compression.
    let dir = scratch_dir("system");
    fs::write(dir.join("AGENTS.md"), "x").expect("write");
    let p = SystemPrompt::build(&dir);
    assert!(p.text.contains("# System"), "{}", p.text);
    assert!(
        p.text.contains("do not re-attempt the same call"),
        "{}",
        p.text
    );
    assert!(p.text.contains("prompt-injection"), "{}", p.text);
    assert!(
        p.text.contains("folded by the system"),
        "mechanism awareness present: {}",
        p.text
    );
}

#[test]
fn test_actions_guides_care() {
    // The actions section must name reversibility + blast radius, the
    // destructive examples (rm -rf, force-push, --no-verify), and the
    // scope-matching rule. Guards the port against silent drift.
    let dir = scratch_dir("actions");
    fs::write(dir.join("AGENTS.md"), "x").expect("write");
    let p = SystemPrompt::build(&dir);
    assert!(
        p.text.contains("# Executing actions with care"),
        "{}",
        p.text
    );
    assert!(
        p.text.contains("reversibility and blast radius"),
        "{}",
        p.text
    );
    assert!(p.text.contains("rm -rf"), "{}", p.text);
    assert!(p.text.contains("--no-verify"), "{}", p.text);
    assert!(p.text.contains("Match the scope"), "{}", p.text);
}

#[test]
fn test_doing_tasks_guides_behavior() {
    // The doing-tasks section must name the core behavioral rules so the
    // model builds a software-engineering framework: read before change,
    // diagnose failures, verify before done, report faithfully, no
    // over-engineering. Guards the port against silent drift.
    let dir = scratch_dir("doing");
    fs::write(dir.join("AGENTS.md"), "x").expect("write");
    let p = SystemPrompt::build(&dir);
    assert!(p.text.contains("# Doing tasks"), "{}", p.text);
    assert!(p.text.contains("Read a file before"), "{}", p.text);
    assert!(
        p.text.contains("diagnose why before switching"),
        "{}",
        p.text
    );
    assert!(p.text.contains("verify it works"), "{}", p.text);
    assert!(p.text.contains("Report outcomes faithfully"), "{}", p.text);
    assert!(p.text.contains("Do not add features"), "{}", p.text);
}

#[test]
fn test_using_tools_guides_parallel() {
    // The using-tools section must name each dedicated tool preference
    // (Read over cat, Grep over grep, Glob over find, Edit over sed,
    // Write over echo) and the compound-command guidance (parallel for
    // independent, && for dependent, no newlines). Guards the port
    // against silent drift back to a vague nudge.
    let dir = scratch_dir("tools");
    fs::write(dir.join("AGENTS.md"), "x").expect("write");
    let p = SystemPrompt::build(&dir);
    assert!(
        p.text.contains("Using your tools"),
        "section header: {}",
        p.text
    );
    assert!(p.text.contains("Read instead of cat"), "{}", p.text);
    assert!(p.text.contains("Grep instead of grep"), "{}", p.text);
    assert!(p.text.contains("Glob instead of find"), "{}", p.text);
    assert!(p.text.contains("Edit instead of sed"), "{}", p.text);
    assert!(
        p.text.contains("independent tool calls in parallel"),
        "{}",
        p.text
    );
    assert!(p.text.contains("&& to chain"), "{}", p.text);
    assert!(
        p.text.contains("Do not use newlines to separate"),
        "{}",
        p.text
    );
}
