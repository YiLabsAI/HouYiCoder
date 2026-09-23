//! Ecosystem skill loading: a skill in the .claude/skills/ directory is
//! discovered, listed, and invocable via the @skill: activation prefix.
//! Proves the cross-ecosystem compatibility path works end-to-end.

use crate::common::{
    self, Key, RENDER_TIMEOUT, fresh_temp_dir, pty_session_with_home, run_skill_command,
};

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

/// A session whose only skill carries a description longer than the pane, so
/// the detail has to wrap it rather than stop at the right edge.
fn long_description_session() -> common::PtySession {
    let home = fresh_temp_dir("wrap-home");
    let repo = fresh_temp_dir("wrap-repo");
    let dir = home.join(".houyicoder").join("skills").join("mock-long");
    std::fs::create_dir_all(&dir).unwrap();
    // Long enough that an unwrapped row clips the tail at any harness width,
    // so the assertion cannot pass on a clipped row.
    let filler = "wrap ".repeat(usize::from(common::COLS));
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: mock-long\ndescription: {filler}zzwrap-tail\n---\nlong body\n"),
    )
    .unwrap();
    let script = r#"[[{"type":"Text","text":"wrap-done"}]]"#;
    pty_session_with_home(repo, home, script)
}

/// The detail wraps a description longer than the pane instead of clipping it
/// at the right edge, so the tail of the text stays readable. A row clipped at
/// the edge drops its tail entirely, so the tail token is what separates a
/// wrapped row from a clipped one.
#[test]
#[ignore]
fn test_long_skill_description_wraps() {
    let mut s = long_description_session();
    s.wait_for("let's build", RENDER_TIMEOUT);
    common::run_slash_command(&mut s, "skills");
    assert!(
        s.wait_for("mock-long", RENDER_TIMEOUT),
        "the skill is listed: {}",
        s.output()
    );
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("zzwrap-tail", RENDER_TIMEOUT),
        "the description tail renders past the pane edge:\n{}",
        s.screen().contents()
    );
    drop(s);
}
