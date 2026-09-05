//! Entitlement approval card E2E over a real PTY: a skill-invoked bash
//! command is denied a mach service, the deny-log scan surfaces it, the
//! two-option card renders, and the verdict writes (or declines) the
//! per-skill grant store in the isolated HOME.
//!
//! macOS-only (seatbelt + the unified log); #[ignore] like the other
//! binary-spawning PTY tests. Run after cargo build --bin houyi:
//! cargo test --test ui_entitlement -- --ignored
#![cfg(target_os = "macos")]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{Key, pty_session_with_home};

/// Looks up a mach service no profile allows, then exits 1 with a
/// denial-signature stderr so the bash discovery gate fires.
const HELPER_C: &str = r#"
#include <mach/mach.h>
#include <servers/bootstrap.h>
#include <stdio.h>
int main(void) {
    mach_port_t port;
    kern_return_t kr = bootstrap_look_up(bootstrap_port, "com.houyi.test.entitlement", &port);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "mach-lookup denied: com.houyi.test.entitlement (%d)\n", kr);
        return 1;
    }
    return 0;
}
"#;

const SERVICE: &str = "com.houyi.test.entitlement";
/// The deny-log scan runs log show; allow seconds, not render ticks.
const ENTITLEMENT_TIMEOUT: Duration = Duration::from_secs(20);

fn make_temp_repo(slug: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "houyi-entitlement-repo-{}-{slug}",
        std::process::id()
    ));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir repo");
    dir
}

fn make_temp_home(slug: u64) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "houyi-entitlement-home-{}-{slug}",
        std::process::id()
    ));
    drop(std::fs::remove_dir_all(&home));
    let skill_dir = home.join(".agents").join("skills").join("test-grant");
    std::fs::create_dir_all(&skill_dir).expect("mkdir skill");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: test-grant\ndescription: Provoke a mach-lookup denial.\n---\nRun the helper.",
    )
    .expect("write SKILL.md");
    home
}

/// Compile the helper into the repo dir. None when no C compiler (skip).
#[allow(clippy::disallowed_methods)]
fn compile_helper(repo: &std::path::Path) -> Option<PathBuf> {
    let src = repo.join("machlookup.c");
    let bin = repo.join("machlookup");
    std::fs::write(&src, HELPER_C).ok()?;
    let out = std::process::Command::new("clang")
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(bin)
}

fn entitlement_script(helper: &std::path::Path) -> String {
    format!(
        r#"[
  [{{"type":"ToolCall","id":"c1","name":"skill","input":{{"skill":"test-grant"}}}}],
  [{{"type":"ToolCall","id":"c2","name":"bash","input":{{"command":"{helper}"}}}}],
  [{{"type":"Text","text":"entitlement-run-done"}}]
]"#,
        helper = helper.display()
    )
}

/// Whitespace-insensitive contains: the PTY diff-redraw draws some lines
/// word-by-word at separate cursor positions, and stripping the ANSI
/// control codes concatenates those words with no space between them.
/// Comparing with all whitespace removed on both sides is stable against
/// both shapes.
fn has_words(haystack: &str, needle: &str) -> bool {
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    squash(haystack).contains(&squash(needle))
}

/// The full chain: the skill call records the active skill, the bash
/// command is mach-denied, the card renders with the two-option layout,
/// Yes writes the grant store.
#[test]
#[ignore = "spawns binary + PTY + clang; macOS seatbelt only"]
fn test_entitlement_yes_grants() {
    let repo = make_temp_repo(1);
    let home = make_temp_home(1);
    let Some(helper) = compile_helper(&repo) else {
        eprintln!("skipping: clang unavailable");
        return;
    };
    let mut s = pty_session_with_home(repo.clone(), home.clone(), &entitlement_script(&helper));
    s.send_str("run it");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("Sandbox entitlement", ENTITLEMENT_TIMEOUT),
        "the entitlement card should render after the mach-denied command:\n{}",
        s.output_plain()
    );
    assert!(
        has_words(&s.output_plain(), "Skill test-grant was blocked from"),
        "the card must name the invoking skill:\n{}",
        s.output_plain()
    );
    assert!(
        s.output_plain().contains(SERVICE),
        "the card must name the blocked service:\n{}",
        s.output_plain()
    );
    assert!(
        has_words(
            &s.output_plain(),
            "Do you want to authorize this service for test-grant?"
        ),
        "the card must ask the authorize question:\n{}",
        s.output_plain()
    );
    assert!(
        s.output_plain().contains("1. Yes") && s.output_plain().contains("2. No"),
        "the card must be two-option:\n{}",
        s.output_plain()
    );
    assert!(
        !s.output_plain().contains("don't ask again"),
        "the remember option must not show on an entitlement card:\n{}",
        s.output_plain()
    );
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("entitlement-run-done", ENTITLEMENT_TIMEOUT),
        "Yes should resume the run to the final reply:\n{}",
        s.output_plain()
    );
    let grants_path = home.join(".houyicoder").join("skill-grants.json");
    let grants = std::fs::read_to_string(&grants_path).unwrap_or_else(|e| {
        panic!(
            "grant store should exist after Yes ({e}): {}",
            grants_path.display()
        )
    });
    assert!(
        grants.contains("test-grant") && grants.contains(SERVICE),
        "the grant store must carry the authorized service:\n{grants}"
    );
    drop(std::fs::remove_dir_all(&repo));
    drop(std::fs::remove_dir_all(&home));
}

/// The decline half: No leaves the grant store untouched and the run ends.
#[test]
#[ignore = "spawns binary + PTY + clang; macOS seatbelt only"]
fn test_entitlement_no_declines() {
    let repo = make_temp_repo(2);
    let home = make_temp_home(2);
    let Some(helper) = compile_helper(&repo) else {
        eprintln!("skipping: clang unavailable");
        return;
    };
    let mut s = pty_session_with_home(repo.clone(), home.clone(), &entitlement_script(&helper));
    s.send_str("run it");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("Sandbox entitlement", ENTITLEMENT_TIMEOUT),
        "the entitlement card should render:\n{}",
        s.output_plain()
    );
    s.send_key(&Key::Char('2'));
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("entitlement-run-done", ENTITLEMENT_TIMEOUT),
        "No should resume the run to the final reply:\n{}",
        s.output_plain()
    );
    let grants_path = home.join(".houyicoder").join("skill-grants.json");
    let written = std::fs::read_to_string(&grants_path).unwrap_or_default();
    assert!(
        !written.contains(SERVICE),
        "No must not authorize the service:\n{written}"
    );
    drop(std::fs::remove_dir_all(&repo));
    drop(std::fs::remove_dir_all(&home));
}
