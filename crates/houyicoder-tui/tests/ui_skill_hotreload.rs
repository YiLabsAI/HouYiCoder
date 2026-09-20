//! Real-binary PTY test for skill hot-reload. #[ignore] (spawns the binary
//! + a PTY). Run via make suite ui after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{self, Command};
use std::time::{Duration, Instant};

use crate::common::{Key, PtySession, RENDER_TIMEOUT, pty_session_in_repo, run_slash_command};

/// Return to the idle input box so the next slash query can be typed. The
/// skills pane replaces the input box, so a leaked Enter would drill into an
/// entry instead of re-querying. Esc is sent until the idle hint renders or
/// the attempts run out; a frame that arrives too late for a short read is
/// absorbed by the caller's retry loop.
fn close_skills_pane(s: &mut PtySession) {
    const IDLE: &str = "let's build, or / for commands";
    for _ in 0..5 {
        if s.screen().contents().contains(IDLE) {
            return;
        }
        s.send_key(&Key::Esc);
        s.wait_for_screen(IDLE, Duration::from_millis(80));
    }
}

/// Throwaway git repo whose seed skill sits in a config family the test does
/// not touch, so the skills directory of the family it later writes to does
/// not exist at start. The empty config directory keeps that family watched
/// from startup, which is what surfaces the new skills directory.
#[allow(clippy::disallowed_methods)]
fn make_hotreload_repo(slug: u64) -> PathBuf {
    let dir = env::temp_dir().join(format!("houyi-skill-hotreload-{}-{slug}", process::id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("mkdir repo");
    fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("write manifest");
    let seed_dir = dir.join(".agents").join("skills").join("seed");
    fs::create_dir_all(&seed_dir).expect("mkdir seed skill");
    fs::write(
        seed_dir.join("SKILL.md"),
        "---\nname: seed\ndescription: seed skill for the watch root\n---\nseed body\n",
    )
    .expect("write seed skill");
    fs::create_dir_all(dir.join(".houyicoder")).expect("mkdir config dir");
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

/// The launcher requires a stub script. This journey drives no turn, so the
/// reply is never rendered and nothing is asserted on it.
const RELOAD_STUB: &str = r#"[
  [{"type":"Text","text":"stub reply"}]
]"#;

/// A skills directory created mid-session is picked up by the hot-reload
/// driver and appears in the live listing. The listing is read from the
/// registry, so the new description there is the reload landing rather than
/// an echo of the typed command.
#[test]
#[ignore]
fn test_hotreload_picks_new_skill() {
    let repo = make_hotreload_repo(1);
    let mut s = pty_session_in_repo(repo.clone(), RELOAD_STUB);
    // A new skills directory reloads without waiting on write stability, so
    // the tree is staged complete and renamed in, and the reload sees every
    // file at once. Creating the directory then the file inside it would land
    // on the debounced path and pay its settle window instead.
    let staged = repo.join("stage-skills").join("newskill");
    fs::create_dir_all(&staged).expect("stage newskill");
    fs::write(
        staged.join("SKILL.md"),
        "---\nname: newskill\ndescription: written mid-session\n---\nnewskill body\n",
    )
    .expect("write staged SKILL.md");
    fs::rename(
        repo.join("stage-skills"),
        repo.join(".houyicoder").join("skills"),
    )
    .expect("rename skills into place");
    // Query the listing until the reload lands, then read the frame that
    // carries it. Both rows are asserted on that one frame: the pane renders
    // a listing, so an echo of the typed command could not carry the seeded
    // skill beside the new one.
    let deadline = Instant::now() + RENDER_TIMEOUT;
    let mut listing = String::new();
    while Instant::now() < deadline {
        run_slash_command(&mut s, "skills");
        if s.wait_for_screen("written mid-session", Duration::from_millis(200)) {
            listing = s.screen().contents();
            break;
        }
        close_skills_pane(&mut s);
    }
    assert!(
        listing.contains("written mid-session")
            && listing.contains("seed skill for the watch root"),
        "the reloaded skills directory should list beside the seeded one:\n{}",
        s.output()
    );
    drop(s);
    fs::remove_dir_all(&repo).ok();
}
