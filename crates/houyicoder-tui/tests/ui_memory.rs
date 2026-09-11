//! Real-binary PTY coverage for memory persistence and pane navigation.
//!
//! Every test uses isolated memory roots and reconstructs terminal state when
//! final layout or styling matters.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, fresh_temp_dir, pty_session_isolated, run_slash_command};
use std::path::PathBuf;

/// A fresh temp HOME the test owns. The memory roots + the settings file land
/// under the project-local state dir inside HOME, so assertions read there
/// + cleanup nukes the whole tree.
///
/// Delegates to fresh_temp_dir so parallel nextest processes cannot mkdir-clash.
fn fresh_home(slug: &str) -> PathBuf {
    fresh_temp_dir(&format!("mem-{slug}"))
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
    let mut s = pty_session_isolated(home.clone());
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for("memory —", RENDER_TIMEOUT),
        "memory pane header should render:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_screen("a to disable auto-memory", RENDER_TIMEOUT),
        "auto-memory action should render:\n{}",
        s.output()
    );
    drop(s);
    drop(std::fs::remove_dir_all(&home));
}

/// /save <key> <source>: <fact> typed as a user message -> the deterministic
/// fact extractor writes a topic file and the next list shows its key.
#[test]
#[ignore]
fn test_save_writes_and_lists() {
    let home = fresh_home("save");
    let mut s = pty_session_isolated(home.clone());
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
    // The save lands just after the run completes (the write races the
    // done-signal); poll the filesystem for the topic rather than a fixed
    // sleep so a slow write does not flake and a fast one does not wait.
    let topic = {
        let deadline = std::time::Instant::now() + RENDER_TIMEOUT;
        loop {
            if let Some(p) = find_topic(&home, "smoke-key") {
                break p;
            }
            if std::time::Instant::now() > deadline {
                panic!("/save did not write the topic file:\n{}", s.output());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    let content = std::fs::read_to_string(&topic).expect("read topic");
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

/// The pane's a shortcut toggles auto-memory and persists the setting.
#[test]
#[ignore]
fn test_toggle_flips_and_persists() {
    let home = fresh_home("toggle");
    let mut s = pty_session_isolated(home.clone());
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for_screen("a to disable auto-memory", RENDER_TIMEOUT),
        "default auto-memory action should render:\n{}",
        s.output()
    );
    s.send_key(&Key::Char('a'));
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

/// Memory tabs, detail navigation, scrolling, and hierarchy keys work through
/// the real terminal.
#[test]
#[ignore]
fn test_pane_navigation() {
    let home = fresh_home("navigation");
    let mut s = pty_session_isolated(home.clone());
    run_slash_command(&mut s, "save nav-key user: memory navigation detail");
    let deadline = std::time::Instant::now() + RENDER_TIMEOUT;
    while find_topic(&home, "nav-key").is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    run_slash_command(&mut s, "memory");
    assert!(s.wait_for_screen("nav-key", RENDER_TIMEOUT));
    s.send_key(&Key::Tab);
    assert!(s.wait_for_screen("[User]", RENDER_TIMEOUT));
    s.send_key(&Key::Left);
    assert!(s.wait_for_screen("[All]", RENDER_TIMEOUT));
    s.send_key(&Key::Enter);
    assert!(s.wait_for_screen("Esc to back", RENDER_TIMEOUT));
    s.send_key(&Key::Down);
    s.send_key(&Key::Esc);
    assert!(s.wait_for_screen("newest first", RENDER_TIMEOUT));
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!s.screen().contents().contains("newest first"));
    drop(s);
    drop(std::fs::remove_dir_all(&home));
}

/// Forget removes the stored topic and the next list reflects the new count.
#[test]
#[ignore]
fn test_forget_deletes_and_refreshes() {
    let home = fresh_home("forget");
    let mut s = pty_session_isolated(home.clone());
    run_slash_command(&mut s, "save forget-key user: never skip tests");
    assert!(
        s.wait_for("done", RENDER_TIMEOUT) || s.wait_for("let's build", RENDER_TIMEOUT),
        "the stub run should complete after /save:\n{}",
        s.output()
    );
    // The save lands just after the run completes (the write races the
    // done-signal); poll the filesystem for the topic rather than a fixed
    // sleep so a slow write does not flake and a fast one does not wait.
    let topic = {
        let deadline = std::time::Instant::now() + RENDER_TIMEOUT;
        loop {
            if let Some(p) = find_topic(&home, "forget-key") {
                break p;
            }
            if std::time::Instant::now() > deadline {
                panic!("save did not write the topic file:\n{}", s.output());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    run_slash_command(&mut s, "memory forget forget-key");
    let deadline = std::time::Instant::now() + RENDER_TIMEOUT;
    while topic.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!topic.exists(), "forget should delete the topic file");
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for_screen("0 stored", RENDER_TIMEOUT),
        "memory pane should show the refreshed count:\n{}",
        s.output()
    );
    drop(s);
    drop(std::fs::remove_dir_all(&home));
}

/// Esc closes the memory pane and removes it from the terminal screen.
#[test]
#[ignore]
fn test_esc_closes_pane() {
    let home = fresh_home("esc");
    let mut s = pty_session_isolated(home.clone());
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for_screen("a to disable auto-memory", RENDER_TIMEOUT),
        "memory pane should render:\n{}",
        s.output()
    );
    s.send_key(&Key::Esc);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let screen = s.screen().contents();
    assert!(!screen.contains("memory —"), "pane should close:\n{screen}");
    assert!(
        !screen.contains("disable auto-memory"),
        "footer should be gone:\n{screen}"
    );
    drop(s);
    drop(std::fs::remove_dir_all(&home));
}
