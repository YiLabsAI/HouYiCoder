use super::*;

use super::history;
use crate::input::queue::InputItem;
use std::env::temp_dir;
use std::fs::{create_dir_all, read_to_string, remove_file};
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[test]
fn test_push_pop_clear() {
    let mut f = InputField::new();
    assert!(f.is_empty());
    f.push('a');
    f.push('b');
    assert_eq!("ab", f.value());
    f.pop();
    assert_eq!("a", f.value());
    let taken = f.take();
    assert_eq!("a", taken);
    assert!(f.is_empty());
}

#[test]
fn test_insert_and_backspace_mid() {
    let mut f = InputField::new();
    f.set("hello".to_string());
    f.move_home();
    f.move_right();
    f.move_right();
    f.insert_str("X");
    assert_eq!("heXllo", f.value());
    f.backspace();
    assert_eq!("hello", f.value());
}

#[test]
fn test_cursor_moves_grapheme_aligned() {
    let mut f = InputField::new();
    f.set("a🚀b".to_string()); // graphemes: a, 🚀, b
    f.move_home();
    f.move_right();
    assert_eq!(f.cursor(), 1); // after 'a'
    f.move_right();
    assert_eq!(f.cursor(), "a🚀".len()); // after 🚀, not mid-codepoint
    f.move_left();
    assert_eq!(f.cursor(), 1);
}

#[test]
fn test_wrap_ascii_by_columns() {
    let mut f = InputField::new();
    f.set("hello world".to_string());
    let lines = f.wrapped_lines(5);
    // "hello" / " worl" / "d"
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].text, "hello");
    assert_eq!(lines[1].text, " worl");
    assert_eq!(lines[2].text, "d");
    assert_eq!(f.line_count(5), 3);
}

#[test]
fn test_wrap_cjk_double_width() {
    let mut f = InputField::new();
    f.set("你好世界".to_string()); // each char width 2
    let lines = f.wrapped_lines(4); // 2 CJK per row
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "你好");
    assert_eq!(lines[1].text, "世界");
}

#[test]
fn test_wrap_preserves_newline_rows() {
    let mut f = InputField::new();
    f.set("a\n\nb".to_string());
    let lines = f.wrapped_lines(80);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].text, "a");
    assert_eq!(lines[1].text, "");
    assert_eq!(lines[2].text, "b");
}

#[test]
fn test_input_height_capped() {
    let mut f = InputField::new();
    assert_eq!(f.input_height(24, 80), 3); // 1 content + 2 border
    f.set("a".repeat(200)); // wraps to many lines at 80 cols
    let h = f.input_height(24, 80);
    assert!(h <= 24 / 2 + 2); // capped near half terminal
    assert!(h > 3);
}

#[test]
fn test_kill_current_line_middle() {
    let mut f = InputField::new();
    f.set("line one\nline two\nline three".to_string());
    // cursor at end (after "three") — kill the last line.
    f.kill_current_line();
    assert_eq!(f.value(), "line one\nline two\n");
    // cursor at the trailing newline position.
    // Now kill the middle line: move cursor onto "line two".
    f.set("line one\nline two\nline three".to_string());
    // place cursor at start of "line two" (offset 9)
    f.cursor = "line one\n".len();
    f.kill_current_line();
    assert_eq!(f.value(), "line one\nline three");
}

#[test]
fn test_kill_only_line() {
    let mut f = InputField::new();
    f.set("only line".to_string());
    f.kill_current_line();
    assert!(f.is_empty());
}

#[test]
fn test_move_home_end_whole() {
    let mut f = InputField::new();
    f.set("hello\nworld".to_string());
    f.move_home();
    assert_eq!(f.cursor(), 0);
    f.move_end();
    assert_eq!(f.cursor(), "hello\nworld".len());
}

#[test]
fn test_move_up_down_preserves() {
    let mut f = InputField::new();
    f.set("hello world foo".to_string()); // wraps at 5: hello /  worl / d foo
    f.move_home();
    f.move_down(5);
    assert_eq!(f.cursor_position(5).0, 1);
    f.move_down(5);
    assert_eq!(f.cursor_position(5).0, 2);
    f.move_up(5);
    assert_eq!(f.cursor_position(5).0, 1);
}

#[test]
fn test_recall_empty_draft() {
    let mut input = Input::default();
    input.merge_recalled_text("recalled".to_string());
    assert_eq!(
        input.editor.value(),
        "recalled",
        "recalled text becomes the draft"
    );
    assert_eq!(
        input.editor.cursor(),
        "recalled".len(),
        "cursor rests at the end"
    );
}

#[test]
fn test_recall_before_draft() {
    let mut input = Input::default();
    input.editor.set("line one\nline two".to_string());
    input.merge_recalled_text("recalled".to_string());
    assert_eq!(
        input.editor.value(),
        "recalled\nline one\nline two",
        "recalled text precedes the existing draft"
    );
    assert_eq!(
        input.editor.cursor(),
        "recalled\n".len(),
        "the cursor sits at the draft boundary, not the end"
    );
}

#[test]
fn test_recall_cjk_boundary() {
    let mut input = Input::default();
    input.editor.set("你好世界".to_string());
    input.merge_recalled_text("recalled".to_string());
    assert_eq!(
        input.editor.cursor(),
        "recalled\n".len(),
        "the cursor lands on the CJK char boundary after the newline"
    );
    input.editor.move_right();
    input.editor.move_right();
    assert_eq!(
        input.editor.cursor(),
        "recalled\n你好".len(),
        "wide chars advance the cursor by their full byte length"
    );
}

#[test]
fn test_reset_composer_keeps_queue() {
    let mut input = Input::default();
    input.editor.set("draft".to_string());
    input.pasted.ingest(&"z".repeat(900));
    input.pending_paste.buffer = Some("chunk".to_string());
    input
        .queue
        .push(InputItem::Command("/resume sid".to_string()));
    input.reset_composer_for_session_switch();
    assert!(input.editor.is_empty(), "the editor clears");
    assert!(input.pasted.is_empty(), "the paste side table clears");
    assert!(
        input.pending_paste.buffer.is_none(),
        "the accumulator clears"
    );
    assert_eq!(
        input.queue.len(),
        1,
        "the queue carries across, not reset here"
    );
}

#[test]
fn test_pending_paste_flushes() {
    let mut input = Input::default();
    input.pending_paste.buffer = Some("chunk".to_string());
    let last = Instant::now();
    input.pending_paste.last = Some(last);
    let token = input.drain_pending_paste(last + Duration::from_millis(50));
    assert_eq!(
        token.as_deref(),
        Some("chunk"),
        "a small chunk is ingested inline"
    );
    assert!(
        input.pending_paste.buffer.is_none(),
        "the buffer is consumed"
    );
    assert!(
        input.pending_paste.last.is_none(),
        "the timestamp is consumed"
    );
}

#[test]
fn test_pending_paste_holds() {
    let mut input = Input::default();
    input.pending_paste.buffer = Some("chunk".to_string());
    let last = Instant::now();
    input.pending_paste.last = Some(last);
    let token = input.drain_pending_paste(last + Duration::from_millis(49));
    assert!(token.is_none(), "a short gap yields no token");
    assert_eq!(
        input.pending_paste.buffer.as_deref(),
        Some("chunk"),
        "the buffer stays for the next check"
    );
    assert!(input.pending_paste.last.is_some(), "the timestamp stays");
}

fn shell_input_with_history(entries: &[&str]) -> Input {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut input = Input::default();
    let dir = temp_dir().join(format!("houyi-shell-hist-{}-{seq}", id()));
    drop(create_dir_all(&dir));
    let path = dir.join("shell_history.jsonl");
    drop(remove_file(&path));
    let project = history::current_project();
    for entry in entries {
        let line = format!("$ {entry}");
        history::add(&line, &project, "", &path);
    }
    input.apply_shell_history_path(path);
    input
}

fn shell_mode_input(entries: &[&str], draft: &str) -> Input {
    let mut input = shell_input_with_history(entries);
    input.mode = InputMode::DirectShell;
    input.editor.set(draft.to_string());
    input
}

/// A leading dollar plus a space enters DirectShell mode and consumes the
/// prefix, so the editor holds the bare command.
#[test]
fn test_dollar_space_enters_shell() {
    let mut input = Input::default();
    input.editor.set("$ cargo test".to_string());
    input.recompute_mode();
    assert_eq!(input.mode, InputMode::DirectShell);
    assert_eq!(input.editor.value(), "cargo test");
}

/// A dollar followed by a letter or digit stays in Chat, so variables and
/// numbers do not false-trigger.
#[test]
fn test_dollar_nonspace_stays_chat() {
    let mut input = Input::default();
    for text in ["$100", "$HOME", "$var"] {
        input.editor.set(text.to_string());
        input.recompute_mode();
        assert_eq!(input.mode, InputMode::Chat, "{text:?} stays in Chat mode");
    }
}

/// A prefix match in the shell history surfaces a ghost suffix.
#[test]
fn test_ghost_from_history() {
    let mut input = shell_mode_input(&["cargo test", "cargo build"], "cargo t");
    input.refresh_ghost(Instant::now());
    assert_eq!(input.ghost.as_deref(), Some("est"));
}

/// Tab accepts the ghost suffix into the editor without executing, and the
/// ghost clears.
#[test]
fn test_tab_accepts_ghost() {
    let mut input = shell_mode_input(&["cargo test"], "cargo t");
    input.refresh_ghost(Instant::now());
    assert!(input.accept_ghost(Instant::now()));
    assert_eq!(input.editor.value(), "cargo test");
    assert!(input.ghost.is_none());
}

/// Esc drops the ghost first; a second Esc exits DirectShell mode, keeping the
/// draft so it can be edited as a normal prompt.
#[test]
fn test_esc_clears_ghost_exits() {
    let mut input = shell_mode_input(&["cargo test"], "cargo t");
    input.refresh_ghost(Instant::now());
    assert!(input.ghost.is_some());
    input.clear_ghost();
    assert!(input.ghost.is_none());
    assert_eq!(
        input.mode,
        InputMode::DirectShell,
        "one Esc clears only the ghost"
    );
    input.exit_shell_mode();
    assert_eq!(input.mode, InputMode::Chat);
    assert_eq!(input.editor.value(), "cargo t", "the draft is kept");
}

/// exit_shell_mode returns the composer to Chat with the draft intact.
#[test]
fn test_backspace_empty_exits_shell() {
    let mut input = Input {
        mode: InputMode::DirectShell,
        ..Default::default()
    };
    input.exit_shell_mode();
    assert_eq!(input.mode, InputMode::Chat);
}

/// record_shell_command prepends to the cache and appends to the prompt-history
/// file, so the command survives a session restart.
#[test]
fn test_record_shell_command_persists() {
    let mut input = shell_input_with_history(&["cargo test"]);
    let path = input.shell_history.path().to_string();
    input.record_shell_command("cargo build");
    let content = read_to_string(&path).unwrap_or_default();
    assert!(
        content.contains("$ cargo build"),
        "the recorded command is appended with the dollar prefix: {content}"
    );
}

/// The ghost and command accessors guard their branches: refresh_ghost clears
/// when not in DirectShell, when the draft is multiline, or when the cursor is
/// mid-text; accept_ghost returns false with no ghost; shell_command returns None
/// in Chat or on an empty draft.
#[test]
fn test_shell_accessors_guard_branches() {
    let now = Instant::now();
    // Chat mode: refresh clears, shell_command is None.
    let mut input = Input {
        ghost: Some("x".into()),
        ..Default::default()
    };
    input.refresh_ghost(now);
    assert!(input.ghost.is_none());
    assert!(input.shell_command().is_none());
    // DirectShell with a multiline draft: refresh clears.
    let mut input = Input {
        mode: InputMode::DirectShell,
        ..Default::default()
    };
    input.editor.set("a\nb".into());
    input.refresh_ghost(now);
    assert!(input.ghost.is_none());
    // DirectShell with cursor mid-text: refresh clears.
    let mut input = Input {
        mode: InputMode::DirectShell,
        ..Default::default()
    };
    input.editor.set("ab".into());
    input.editor.move_left();
    input.refresh_ghost(now);
    assert!(input.ghost.is_none());
    // accept_ghost with no ghost is a no-op.
    let mut input = Input {
        mode: InputMode::DirectShell,
        ..Default::default()
    };
    assert!(!input.accept_ghost(now));
    // DirectShell with an empty draft: shell_command is None.
    let input = Input {
        mode: InputMode::DirectShell,
        ..Default::default()
    };
    assert!(input.shell_command().is_none());
    // recompute_mode is a no-op when already in DirectShell.
    let mut input = Input {
        mode: InputMode::DirectShell,
        ..Default::default()
    };
    input.editor.set("cargo".into());
    input.recompute_mode();
    assert_eq!(input.editor.value(), "cargo");
}
