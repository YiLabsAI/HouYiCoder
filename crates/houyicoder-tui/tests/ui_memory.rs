//! Real-binary PTY coverage for memory persistence and pane navigation.
//!
//! Every test uses isolated memory roots and reconstructs terminal state when
//! final layout or styling matters.

#![allow(clippy::unwrap_in_result)]

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::common::{
    Key, RENDER_TIMEOUT, fresh_temp_dir, pty_session_isolated, pty_session_scripted_home,
    pty_session_slow_scripted_home, run_slash_command,
};

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
fn find_topic(home: &Path, key: &str) -> Option<PathBuf> {
    let needle = format!("{key}.md");
    let mut stack = vec![home.join(".houyicoder")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
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
        s.wait_for("memories · newest first", RENDER_TIMEOUT),
        "memory pane header should render:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_screen("a to toggle ● auto-memory", RENDER_TIMEOUT),
        "auto-memory action should render:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_screen("● auto-memory", RENDER_TIMEOUT),
        "both switches default on in the header status row:\n{}",
        s.output()
    );
    drop(s);
    drop(fs::remove_dir_all(&home));
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
    // The write lands after the run completes, so polling the topic file is
    // the completion signal and the assertion at once. A fixed sleep would
    // flake on a slow write and idle on a fast one.
    let topic = {
        let deadline = Instant::now() + RENDER_TIMEOUT;
        loop {
            if let Some(p) = find_topic(&home, "smoke-key") {
                break p;
            }
            if Instant::now() > deadline {
                panic!("the /save run should write the topic file:\n{}", s.output());
            }
            thread::sleep(Duration::from_millis(20));
        }
    };
    let content = fs::read_to_string(&topic).expect("read topic");
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
    drop(fs::remove_dir_all(&home));
}

/// The pane's a shortcut toggles auto-memory and persists the setting.
#[test]
#[ignore]
fn test_toggle_flips_and_persists() {
    let home = fresh_home("toggle");
    let mut s = pty_session_isolated(home.clone());
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for_screen("● auto-memory", RENDER_TIMEOUT),
        "default state renders on in the header:\n{}",
        s.output()
    );
    s.send_key(&Key::Char('a'));
    // The reply flips the header glyph off — the pane-visible proof that
    // the round-trip landed.
    assert!(
        s.wait_for_screen("○ auto-memory", RENDER_TIMEOUT),
        "the toggle reply should flip the header to off:\n{}",
        s.output()
    );
    // The server flips + persists; the settings file is the durable proof.
    let settings = home.join(".houyicoder").join("settings.json");
    let deadline = Instant::now() + RENDER_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(content) = fs::read_to_string(&settings)
            && (content.contains("\"auto_memory\":false")
                || content.contains("\"auto_memory\": false"))
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let content = fs::read_to_string(&settings).unwrap_or_else(|_| String::new());
    assert!(
        content.contains("\"auto_memory\":false") || content.contains("\"auto_memory\": false"),
        "toggle should persist auto_memory=false to the settings file:\n{content}\n{}",
        s.output()
    );
    drop(s);
    drop(fs::remove_dir_all(&home));
}

/// Memory tabs, detail navigation, scrolling, and hierarchy keys work through
/// the real terminal.
#[test]
#[ignore]
fn test_pane_navigation() {
    let home = fresh_home("navigation");
    let mut s = pty_session_isolated(home.clone());
    run_slash_command(&mut s, "save nav-key user: memory navigation detail");
    let deadline = Instant::now() + RENDER_TIMEOUT;
    while find_topic(&home, "nav-key").is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    run_slash_command(&mut s, "memory");
    assert!(s.wait_for_screen("nav-key", RENDER_TIMEOUT));
    s.send_key(&Key::Tab);
    assert!(s.wait_for_screen("[User]", RENDER_TIMEOUT));
    s.send_key(&Key::Left);
    assert!(s.wait_for_screen("[All]", RENDER_TIMEOUT));
    s.send_key(&Key::Enter);
    assert!(s.wait_for_screen("Esc to back", RENDER_TIMEOUT));
    assert!(s.wait_for_screen("source: user · updated:", RENDER_TIMEOUT));
    s.send_key(&Key::Down);
    assert!(
        s.wait_for_screen("source: user · updated:", RENDER_TIMEOUT),
        "detail metadata must remain fixed while the body scrolls:\n{}",
        s.output()
    );
    s.send_key(&Key::Esc);
    assert!(s.wait_for_screen("newest first", RENDER_TIMEOUT));
    s.send_key(&Key::Esc);
    thread::sleep(Duration::from_millis(200));
    assert!(!s.screen().contents().contains("newest first"));
    drop(s);
    drop(fs::remove_dir_all(&home));
}

/// Forget removes the stored topic and the next list reflects the new count.
#[test]
#[ignore]
fn test_forget_deletes_and_refreshes() {
    let home = fresh_home("forget");
    let mut s = pty_session_isolated(home.clone());
    run_slash_command(&mut s, "save forget-key user: never skip tests");
    // The write lands after the run completes, so polling the topic file is
    // the completion signal and the assertion at once. A fixed sleep would
    // flake on a slow write and idle on a fast one.
    let topic = {
        let deadline = Instant::now() + RENDER_TIMEOUT;
        loop {
            if let Some(p) = find_topic(&home, "forget-key") {
                break p;
            }
            if Instant::now() > deadline {
                panic!("the /save run should write the topic file:\n{}", s.output());
            }
            thread::sleep(Duration::from_millis(20));
        }
    };
    run_slash_command(&mut s, "memory forget forget-key");
    let deadline = Instant::now() + RENDER_TIMEOUT;
    while topic.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!topic.exists(), "forget should delete the topic file");
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for_screen("0 memories", RENDER_TIMEOUT),
        "memory pane should show the refreshed count:\n{}",
        s.output()
    );
    drop(s);
    drop(fs::remove_dir_all(&home));
}

/// Esc closes the memory pane and removes it from the terminal screen.
#[test]
#[ignore]
fn test_esc_closes_pane() {
    let home = fresh_home("esc");
    let mut s = pty_session_isolated(home.clone());
    run_slash_command(&mut s, "memory");
    assert!(
        s.wait_for_screen("a to toggle ● auto-memory", RENDER_TIMEOUT),
        "memory pane should render:\n{}",
        s.output()
    );
    s.send_key(&Key::Esc);
    thread::sleep(Duration::from_millis(300));
    let screen = s.screen().contents();
    assert!(
        !screen.contains("newest first"),
        "pane should close:\n{screen}"
    );
    assert!(
        !screen.contains("a to toggle ● auto-memory"),
        "memory controls should be gone:\n{screen}"
    );
    drop(s);
    drop(fs::remove_dir_all(&home));
}

/// A save_memory call the model answers with in the main run drains when the
/// turn settles
/// into a PrimaryAgent notice. The notice summary carries the producer label
/// (saved by the agent) and the count; the expanded per-change row carries
/// the scope the write was addressed to. Pins the producer-label summary and
/// the scope row through the real binary, not a TestBackend.
#[test]
#[ignore]
fn test_notice_label_and_scope() {
    let home = fresh_home("notice-scope");
    // The main agent's save_memory tool is unpinned, so the input carries a
    // scope field the model picks per call. A project scope lands the write
    // in the project root and the notice names it.
    let script = r#"[
      [{"type":"ToolCall","id":"c1","name":"save_memory","input":{"key":"deploy-gate","description":"deploy gate state","source":"feedback","content":"The deploy gate is red.","scope":"project"}}],
      [{"type":"Text","text":"saved"}]
    ]"#;
    let mut s = pty_session_scripted_home(script, home.clone());
    s.send_str("note the deploy gate");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("saved by the agent", RENDER_TIMEOUT),
        "the notice summary should carry the producer label:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("1 change", RENDER_TIMEOUT),
        "the summary should name the change count:\n{}",
        s.output()
    );
    // The fold layer collapses the notice to its summary; Ctrl+O reveals the
    // per-change row, which carries the key and the scope.
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for("created deploy-gate", RENDER_TIMEOUT),
        "the expanded row should name the key:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("scope: project", RENDER_TIMEOUT),
        "the expanded row should name the scope the write was addressed to:\n{}",
        s.output()
    );
    drop(s);
    drop(fs::remove_dir_all(&home));
}

/// A background extractor notice lands in the transcript after the run that
/// triggered it, with the producer label and the auto scope the extractor's
/// pinned save carries. Covers the real order a background notice arrives in
/// after the user's turn, end-to-end through the real binary, not a synthetic
/// TestBackend injection. The pane-open path is covered by the unit tests;
/// this test pins the live notice itself.
#[test]
#[ignore]
fn test_extractor_notice_lands() {
    let home = fresh_home("notice-bg");
    // The main run is a plain reply so it ends in one model call; the
    // extractor's fork then consumes the scripted save_memory (with an
    // evidence quote drawn from the prompt) and a trailing text so the pass
    // ends. The slow stub keeps the extractor in flight so the notice lands
    // measurably after the run.
    let script = r#"[
      [{"type":"Text","text":"ok"}],
      [{"type":"ToolCall","id":"ex1","name":"save_memory","input":{"key":"gate-fact","description":"gate state","source":"feedback","content":"The gate is open.","evidence":[{"quote":"note the gate"}]}}],
      [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_slow_scripted_home(600, script, home.clone());
    s.send_str("note the gate state");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("ok", RENDER_TIMEOUT),
        "the main run should finish before the extractor fires:\n{}",
        s.output()
    );
    // The extractor's notice lands after the run with the producer label and
    // the auto scope the pinned save carries.
    assert!(
        s.wait_for("extracted in background", RENDER_TIMEOUT),
        "the background notice should carry the producer label:\n{}",
        s.output()
    );
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for("created gate-fact · scope: auto", RENDER_TIMEOUT),
        "the expanded row should name the key and the auto scope:\n{}",
        s.output()
    );
    drop(s);
    drop(fs::remove_dir_all(&home));
}
