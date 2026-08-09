//! Real-binary PTY smoke tests for the /memory pane. #[ignore] (each spawns
//! the houyi binary + a PTTY -- too slow for the 60s commit gate). Run via
//! make test ui (builds the bin first) or
//! cargo test --test ui_memory -- --ignored after cargo build --bin houyi.
//!
//! Industrial-usability proof for the memory loop: launch the real binary,
//! drive /memory + /save + the toggle / forget / esc sub-paths through a real
//! terminal, assert BOTH the rendered output AND the real on-disk side-effects
//! (memory topic files + the settings.json toggle persistence). The inline
//! unit layer + the wire-contract layer prove the mechanism; this layer proves
//! a user can actually use memory end to end. HOME is overridden to a temp dir
//! so /save never touches the developer's real home.
//!
//! Render-flip assertions use the ANSI-stripped PLAIN output (the value span is
//! a separate styled span, so the raw stream splits "Auto-memory: on" across
//! an SGR run). Persistence + delete assertions read real files (the durable
//! side-effects) — stronger than screen matching, which ratatui's incremental
//! diff-draw would otherwise fragment.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, run_slash_command, session_on_working_with_home};
use std::path::PathBuf;

/// A fresh temp HOME the test owns. The memory roots + the settings file land
/// under the project-local state dir inside HOME, so assertions read there
/// + cleanup nukes the whole tree.
fn fresh_home(slug: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "houyi-ui-mem-{}-{}-{}",
        slug,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir(&p).expect("mkdir temp home");
    p
}

/// Walk the state dir under HOME for a topic file named <key>.md. The /save
/// write lands in the auto-scope root under a project-slug subdir; the slug
/// varies by workspace, so glob the tree rather than hardcode the path.
fn find_topic(home: &std::path::Path, key: &str) -> Option<PathBuf> {
    let needle = format!("{key}.md");
    let mut stack = vec![home.join(".houyicoder")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy() == needle)
            {
                return Some(path);
            }
        }
    }
    None
}

/// The /memory pane header renders + the two toggle rows are visible. The
/// real slash palette -> pane path, not a render assertion on a TestBackend.
#[test]
#[ignore]
fn test_pane_opens() {
    let home = fresh_home("open");
    let mut s = session_on_working_with_home(home.clone());
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for("memory —", RENDER_TIMEOUT),
        "memory pane header should render:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("Auto-memory:"),
        "auto-memory toggle row should render:\n{}",
        s.output()
    );
    drop(s);
    drop(std::fs::remove_dir_all(&home));
}

/// /save <key> <source>: <fact> typed as a user message -> the deterministic
/// fact extractor writes a topic file to the auto-scope root, and the next
/// /memory listing shows the key. Pins the write path + the list refresh
/// through the real binary.
#[test]
#[ignore]
fn test_save_writes_and_lists() {
    let home = fresh_home("save");
    let mut s = session_on_working_with_home(home.clone());
    // /save is a user message (not a slash command): the extractor pattern
    // matches it after the run completes. Type it as the message body.
    run_slash_command(&mut s, "save smoke-key user: always run make check");
    // The stub run completes; the fact is written after. Wait for the run
    // to settle (the stub's final text) before checking disk.
    assert!(
        s.wait_for("done", RENDER_TIMEOUT) || s.wait_for("let's build", RENDER_TIMEOUT),
        "the stub run should complete after /save:\n{}",
        s.output()
    );
    // Give the post-run fact write a beat (best-effort; the write lands
    // after the run).
    std::thread::sleep(std::time::Duration::from_millis(300));
    let topic = find_topic(&home, "smoke-key");
    assert!(
        topic.is_some(),
        "/save should write a topic file under the state dir:\n{}",
        s.output()
    );
    let content = std::fs::read_to_string(topic.unwrap()).expect("read topic");
    assert!(
        content.contains("make check"),
        "the topic body should carry the saved fact:\n{content}"
    );
    // /memory lists the saved key.
    s.clear_output();
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for("smoke-key", RENDER_TIMEOUT),
        "/memory should list the saved key:\n{}",
        s.output()
    );
    drop(s);
    drop(std::fs::remove_dir_all(&home));
}

/// /memory toggle auto flips the auto-memory row on -> off and persists the
/// choice to the settings file. The render flip is diff-drawn (the value span
/// is overwritten in place, so "Auto-memory: off" never exists as a contiguous
/// substring), so the durable signal is the settings file write — the real
/// industrial-grade proof that the toggle wire round-trip + the persistence
/// path land through the real binary.
#[test]
#[ignore]
fn test_toggle_flips_and_persists() {
    let home = fresh_home("toggle");
    let mut s = session_on_working_with_home(home.clone());
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for_plain("Auto-memory: on", RENDER_TIMEOUT),
        "default auto-memory should be on:\n{}",
        s.output()
    );
    s.clear_output();
    // Close the memory pane first: while the pane is open, the slash key
    // does not open the command palette (the pane intercepts input).
    s.send_key(&Key::Esc);
    // Wait for the pane to close + the working screen to return before
    // sending the toggle command through the palette.
    s.wait_for("let's build, or / for commands", RENDER_TIMEOUT);
    run_slash_command(&mut s, "memory toggle auto");
    // The server flips + persists; the settings file is the durable proof.
    let settings = home.join(".houyicoder").join("settings.json");
    let deadline = std::time::Instant::now() + RENDER_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(&settings)
            && (content.contains("\"auto_memory\":false")
                || content.contains("\"auto_memory\": false"))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let content = std::fs::read_to_string(&settings).unwrap_or_else(|_| String::new());
    assert!(
        content.contains("\"auto_memory\":false") || content.contains("\"auto_memory\": false"),
        "toggle should persist auto_memory=false to the settings file:\n{content}\n{}",
        s.output()
    );
    drop(s);
    drop(std::fs::remove_dir_all(&home));
}

/// /memory forget <key> deletes the topic file on disk AND refreshes the pane
/// to "0 stored". Pins the delete wire round-trip + the write-root resolution
/// under a temp HOME (the save + the forget must hit the same root).
#[test]
#[ignore]
fn test_forget_deletes_and_refreshes() {
    let home = fresh_home("forget");
    let mut s = session_on_working_with_home(home.clone());
    run_slash_command(&mut s, "save forget-key user: never skip tests");
    assert!(
        s.wait_for("done", RENDER_TIMEOUT) || s.wait_for("let's build", RENDER_TIMEOUT),
        "the stub run should complete after /save:\n{}",
        s.output()
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let topic = find_topic(&home, "forget-key").expect("save wrote the topic file");
    run_slash_command(&mut s, "memory forget forget-key");
    assert!(
        s.wait_for("memory: 0 stored", RENDER_TIMEOUT),
        "forget should refresh the list to 0 stored:\n{}",
        s.output()
    );
    assert!(
        !topic.exists(),
        "forget should delete the topic file from disk:\n{}",
        s.output()
    );
    drop(s);
    drop(std::fs::remove_dir_all(&home));
}

/// Esc closes the /memory pane back to the transcript. The pane footer
/// advertises "Esc close" so the key must actually dismiss the pane.
#[test]
#[ignore]
fn test_esc_closes_pane() {
    let home = fresh_home("esc");
    let mut s = session_on_working_with_home(home.clone());
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for("Auto-memory:", RENDER_TIMEOUT),
        "memory pane should render:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_key(&Key::Esc);
    // After Esc the pane is dismissed: the toggle rows + the "Esc close"
    // footer are gone from the cleared output window. The working-screen
    // input placeholder is NOT re-rendered (ratatui diff leaves an unchanged
    // input box untouched), so the reliable signal is the ABSENCE of the
    // memory pane rows, not the presence of a working-screen marker. A late
    // list/toggle response can no longer reopen the pane (the result
    // handlers guard the reopen on pane == Memory), so no drain sleep is
    // needed before Esc.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !s.output_plain().contains("Auto-memory:"),
        "Esc should close the memory pane (toggle row gone):\n{}",
        s.output()
    );
    drop(s);
    drop(std::fs::remove_dir_all(&home));
}
