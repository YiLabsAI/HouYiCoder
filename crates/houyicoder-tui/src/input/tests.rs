use super::*;

use crate::input::queue::InputItem;
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
