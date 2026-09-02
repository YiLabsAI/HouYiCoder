//! Real-binary PTY smoke for paths-gated skill activation. #[ignore]
//! (spawns the binary + a PTY). Run via make test ui or
//! cargo test --test ui_skill_paths -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use std::path::PathBuf;
use std::process::Command;

use common::{Key, RENDER_TIMEOUT, pty_session_in_repo, run_slash_command};

/// Throwaway git repo with a paths-gated skill plus a matching file under
/// src/ and a non-matching one under other/.
#[allow(clippy::disallowed_methods)]
fn make_skill_repo(slug: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("houyi-skill-paths-{}-{slug}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir repo");
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("write manifest");
    let skill_dir = dir.join(".houyicoder").join("skills").join("gated");
    std::fs::create_dir_all(&skill_dir).expect("mkdir skill");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: gated\ndescription: paths-gated fixture\npaths:\n  - src/**\n---\ngated body\n",
    )
    .expect("write skill");
    std::fs::create_dir_all(dir.join("src")).expect("mkdir src");
    std::fs::write(dir.join("src").join("foo.rs"), "fn foo() {}\n").expect("write src/foo.rs");
    std::fs::create_dir_all(dir.join("other")).expect("mkdir other");
    std::fs::write(dir.join("other").join("x.rs"), "fn x() {}\n").expect("write other/x.rs");
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@x"][..],
        &["config", "user.name", "t"][..],
        &["add", "-A"][..],
        &["commit", "-m", "init", "-q"][..],
    ] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {:?}", args);
    }
    dir
}

/// Read src/foo.rs, end the run, then a reply for the post-activation call.
const TOUCH_SRC_SCRIPT: &str = r#"[
  [{"type":"ToolCall","id":"c1","name":"read","input":{"path":"src/foo.rs"}}],
  [{"type":"Text","text":"read-done"}],
  [{"type":"Text","text":"gated-passed"}]
]"#;

/// /gated refuses until a matching file touch activates it, then passes.
#[test]
#[ignore]
fn test_conditional_skill_activation_flow() {
    let repo = make_skill_repo(1);
    let mut s = pty_session_in_repo(repo.clone(), TOUCH_SRC_SCRIPT);
    run_slash_command(&mut s, "gated");
    assert!(
        s.wait_for_compact("touchamatchingfile", RENDER_TIMEOUT),
        "refuse before touch should name the paths:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_str("read the source");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_compact("read-done", RENDER_TIMEOUT),
        "the read run should complete:\n{}",
        s.output()
    );
    s.clear_output();
    run_slash_command(&mut s, "gated");
    // After a matching touch the skill runs: a run starts and the stub's
    // third reply lands. A still-refused skill makes no model call, so the
    // reply never arrives -- that is the discriminator. Absence of the
    // refusal text is not, since the transcript retains the earlier line.
    assert!(
        s.wait_for_compact("gated-passed", RENDER_TIMEOUT),
        "the skill should run after a matching touch:\n{}",
        s.output()
    );
    drop(s);
    std::fs::remove_dir_all(&repo).ok();
}

/// A non-matching file touch does not activate; /gated still refuses.
#[test]
#[ignore]
fn test_nonmatching_touch_keeps_refusal() {
    let repo = make_skill_repo(2);
    let script = r#"[
  [{"type":"ToolCall","id":"c1","name":"read","input":{"path":"other/x.rs"}}],
  [{"type":"Text","text":"read-done"}]
]"#;
    let mut s = pty_session_in_repo(repo.clone(), script);
    s.send_str("read the other file");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_compact("read-done", RENDER_TIMEOUT),
        "the read run should complete:\n{}",
        s.output()
    );
    s.clear_output();
    run_slash_command(&mut s, "gated");
    assert!(
        s.wait_for_compact("touchamatchingfile", RENDER_TIMEOUT),
        "a non-matching touch must not activate:\n{}",
        s.output()
    );
    drop(s);
    std::fs::remove_dir_all(&repo).ok();
}
