//! Real-binary PTY smoke for skill hot-reload. #[ignore] (spawns the binary
//! + a PTY). Run via make test ui after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use std::path::PathBuf;
use std::process::Command;

use common::{RENDER_TIMEOUT, pty_session_in_repo, run_skill_command};

/// Throwaway git repo with one seed skill (alpha) so the hot-reload driver
/// has a real skills directory to watch deeply. newskill is NOT present at
/// start — the test writes it mid-session and asserts it becomes invocable
/// after reload.
#[allow(clippy::disallowed_methods)]
fn make_hotreload_repo(slug: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "houyi-skill-hotreload-{}-{slug}",
        std::process::id()
    ));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir repo");
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("write manifest");
    let alpha_dir = dir.join(".houyicoder").join("skills").join("alpha");
    std::fs::create_dir_all(&alpha_dir).expect("mkdir alpha skill");
    std::fs::write(
        alpha_dir.join("SKILL.md"),
        "---\nname: alpha\ndescription: seed skill for the watch root\n---\nalpha body\n",
    )
    .expect("write alpha skill");
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

/// The stub model replies once: /newskill (after reload) injects the skill
/// body as a user message, the model is called, the stub acknowledges.
const HOTRELOAD_SCRIPT: &str = r#"[
  [{"type":"Text","text":"newskillpassed"}]
]"#;

/// A skill written mid-session is picked up by the hot-reload driver and
/// becomes invocable: write newskill after start, wait for the reload
/// (debounce + write-stability), then /newskill runs.
#[test]
#[ignore]
fn test_hotreload_picks_new_skill() {
    let repo = make_hotreload_repo(1);
    let mut s = pty_session_in_repo(repo.clone(), HOTRELOAD_SCRIPT);
    // Write a new skill mid-session. The driver watches the skills directory
    // deeply; the new SKILL.md fires a reload after debounce + stability.
    let newskill_dir = repo.join(".houyicoder").join("skills").join("newskill");
    std::fs::create_dir_all(&newskill_dir).expect("mkdir newskill");
    std::fs::write(
        newskill_dir.join("SKILL.md"),
        "---\nname: newskill\ndescription: written mid-session\n---\nnewskill body\n",
    )
    .expect("write newskill");
    // Wait for the reload to settle: debounce (300ms) + write-stability (1s)
    // + re-discover. 3s is a safe bound for a ~1.4s deterministic reload.
    std::thread::sleep(std::time::Duration::from_secs(3));
    run_skill_command(&mut s, "newskill");
    assert!(
        s.wait_for_compact("newskillpassed", RENDER_TIMEOUT),
        "newskill written mid-session should be invocable after hot-reload:\n{}",
        s.output()
    );
    drop(s);
    std::fs::remove_dir_all(&repo).ok();
}
