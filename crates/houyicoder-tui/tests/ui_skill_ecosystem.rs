//! Ecosystem skill loading: a skill in the .claude/skills/ directory is
//! discovered, listed, and invocable via the @skill: activation prefix.
//! Proves the cross-ecosystem compatibility path works end-to-end.

#![cfg(test)]

mod common;

use common::{RENDER_TIMEOUT, fresh_temp_dir, pty_session_with_home, run_skill_command};

/// Build a PTY session with a temp HOME (containing a .claude/skills/
/// ecosystem skill) and a temp workspace (containing a native skill).
fn ecosystem_session() -> common::PtySession {
    let home = fresh_temp_dir("eco-home");
    let repo = fresh_temp_dir("eco-repo");
    let eco_dir = home.join(".claude").join("skills").join("mock-eco");
    std::fs::create_dir_all(&eco_dir).unwrap();
    std::fs::write(
        eco_dir.join("SKILL.md"),
        "---\nname: mock-eco\ndescription: an ecosystem skill\n---\neco body\n",
    )
    .unwrap();
    let native_dir = home.join(".houyicoder").join("skills").join("mock-native");
    std::fs::create_dir_all(&native_dir).unwrap();
    std::fs::write(
        native_dir.join("SKILL.md"),
        "---\nname: mock-native\ndescription: a native skill\n---\nnative body\n",
    )
    .unwrap();
    let script = r#"[[{"type":"Text","text":"eco-done"}]]"#;
    pty_session_with_home(repo, home, script)
}

/// Both ecosystem and native skills are discovered and appear in the
/// /skills listing.
#[test]
#[ignore]
fn test_ecosystem_skill_discovered() {
    let mut s = ecosystem_session();
    s.wait_for("let's build", RENDER_TIMEOUT);
    common::run_slash_command(&mut s, "skills");
    assert!(
        s.wait_for("mock-eco", RENDER_TIMEOUT),
        "ecosystem skill discovered: {}",
        s.output()
    );
    assert!(
        s.output().contains("mock-native"),
        "native skill discovered: {}",
        s.output()
    );
    drop(s);
}

/// @skill:mock-eco activates the ecosystem skill — the stub provider
/// responds, proving the skill was resolved (not refused as unknown).
#[test]
#[ignore]
fn test_ecosystem_skill_invoke() {
    let mut s = ecosystem_session();
    run_skill_command(&mut s, "mock-eco");
    assert!(
        s.wait_for("eco-done", RENDER_TIMEOUT),
        "ecosystem skill invoked (stub replied): {}",
        s.output()
    );
    assert!(
        s.output().contains("@skill:mock-eco"),
        "user echo has skill prefix: {}",
        s.output()
    );
    drop(s);
}
