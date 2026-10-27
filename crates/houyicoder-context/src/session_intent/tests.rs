//! Tests for this module.

use super::{TitleSource, apply_prompt, apply_prompt_backfill, display_title, prompt_intent};
use crate::descriptor::{
    NameSource, PromptCoverage, PromptRecord, SessionDescriptor, SessionProvenance,
};
use crate::ids::EventId;
use std::sync::atomic::{AtomicU8, Ordering};

/// Hands every record in a test run a later event id than the one before, so
/// the ordering the store applies is the ordering these tests assert on.
static NEXT_TICK: AtomicU8 = AtomicU8::new(1);

fn record(text: &str) -> PromptRecord {
    let tick = NEXT_TICK.fetch_add(1, Ordering::Relaxed);
    let base = "01J00000000000000000000000";
    PromptRecord {
        text: text.to_string(),
        event_id: EventId::from_display_string(&format!("{}{tick:02}", &base[..24]))
            .expect("valid ulid"),
    }
}

fn fresh() -> SessionDescriptor {
    SessionDescriptor::delegated_child(SessionProvenance::Fresh, 0, PromptCoverage::Complete)
}

#[test]
fn test_prompt_keeps_task() {
    assert_eq!(
        prompt_intent("  fix   the   flaky login test  ").as_deref(),
        Some("fix the flaky login test")
    );
}

#[test]
fn test_prompt_skips_greeting() {
    for ack in ["hi", "Thanks!", "好的。", "OK", "嗯"] {
        assert_eq!(prompt_intent(ack), None, "{ack} states no task");
    }
}

#[test]
fn test_prompt_skips_bare_continuation() {
    for ack in ["continue", "Continue.", "go on", "keep going"] {
        assert_eq!(prompt_intent(ack), None, "{ack} states no task");
    }
}

#[test]
fn test_prompt_skips_short_reply() {
    for ack in [
        "yes",
        "No.",
        "sure",
        "got it",
        "Sounds good!",
        "hello there",
        "perfect",
        "Great!",
        "nice",
    ] {
        assert_eq!(prompt_intent(ack), None, "{ack} states no task");
    }
    // The same reply in Chinese is one word, not one character: an approval
    // is an answer to the previous message, never a subject of its own.
    for ack in ["可以", "行", "好", "没问题", "对的", "就这样"] {
        assert_eq!(prompt_intent(ack), None, "{ack} states no task");
    }
}

#[test]
fn test_prompt_keeps_longer_continuation() {
    assert_eq!(
        prompt_intent("continue the migration to the new config").as_deref(),
        Some("continue the migration to the new config"),
        "a continuation word opens a task when the rest says what to do"
    );
}

#[test]
fn test_prompt_skips_slash_command() {
    assert_eq!(prompt_intent("/resume"), None);
    assert_eq!(prompt_intent("/model opus"), None);
    assert_eq!(prompt_intent("/status"), None);
}

#[test]
fn test_prompt_keeps_path_subject() {
    assert_eq!(
        prompt_intent("/Users/me/src/main.rs fails to build").as_deref(),
        Some("/Users/me/src/main.rs fails to build"),
        "a path the frontend forwards to the model is a task statement"
    );
    assert_eq!(
        prompt_intent("/home/you/sample-project").as_deref(),
        Some("/home/you/sample-project")
    );
}

#[test]
fn test_prompt_skips_metadata_block() {
    assert_eq!(
        prompt_intent("<ide_opened_file>src/main.rs</ide_opened_file>"),
        None
    );
}

#[test]
fn test_prompt_strips_metadata_wraps() {
    assert_eq!(
        prompt_intent("<ide_opened_file>src/main.rs</ide_opened_file> fix the failing test")
            .as_deref(),
        Some("fix the failing test"),
        "editor context before the user's words does not hide the task"
    );
    assert_eq!(
        prompt_intent("refactor the parser <system-reminder>hook output</system-reminder>")
            .as_deref(),
        Some("refactor the parser"),
        "a hook note after the words is not part of the subject"
    );
    assert_eq!(
        prompt_intent(
            "<command-name>/model</command-name>\
             <local-command-stdout>ok</local-command-stdout> explain the diff"
        )
        .as_deref(),
        Some("explain the diff"),
        "adjacent blocks are all removed"
    );
    assert_eq!(
        prompt_intent("fix the <button>click</button> layout").as_deref(),
        Some("fix the <button>click</button> layout"),
        "a tag outside the metadata families is prose"
    );
}

/// A metadata block larger than the recognized bound still does not become
/// the title: the text after its opening tag is block content, not prose.
#[test]
fn test_prompt_drops_oversized_metadata() {
    let body = "x".repeat(5000);
    let whole = format!("<ide_selection>{body}</ide_selection>");
    assert_eq!(prompt_intent(&whole), None);
    let with_words = format!("fix the build <ide_selection>{body}");
    assert_eq!(prompt_intent(&with_words).as_deref(), Some("fix the build"));
}

/// A paste full of angle brackets says nothing, and deciding that must not
/// cost a block-sized scan per bracket: the tag-name rules each one out.
#[test]
fn test_prompt_skips_char_run() {
    let run = "<".repeat(64 * 1024);
    assert_eq!(prompt_intent(&run), None);
}

#[test]
fn test_prompt_keeps_code_task() {
    assert_eq!(
        prompt_intent("<T> is not Send when held across an await").as_deref(),
        Some("<T> is not Send when held across an await"),
        "an angle bracket alone does not make input metadata"
    );
}

#[test]
fn test_prompt_skips_empty() {
    assert_eq!(prompt_intent(""), None);
    assert_eq!(prompt_intent("   \n\t "), None);
}

#[test]
fn test_prompt_skips_marks_only() {
    assert_eq!(prompt_intent("..."), None);
    assert_eq!(prompt_intent("!!!"), None);
}

#[test]
fn test_prompt_strips_control_sequences() {
    assert_eq!(
        prompt_intent("\u{1b}[31mfix the build\u{1b}[0m").as_deref(),
        Some("fix the build")
    );
    assert_eq!(
        prompt_intent("a\u{7}b").as_deref(),
        Some("a b"),
        "a control character becomes a space, not a hole"
    );
}

#[test]
fn test_prompt_strips_window_title() {
    assert_eq!(
        prompt_intent("\u{1b}]0;my title\u{7}fix the build").as_deref(),
        Some("fix the build"),
        "an OSC title ended by a bell is not text"
    );
    assert_eq!(
        prompt_intent("\u{1b}]8;;http://example.com\u{1b}\\see the log").as_deref(),
        Some("see the log"),
        "an OSC link ended by a string terminator is not text"
    );
    assert_eq!(
        prompt_intent("\u{1b}cfix the build").as_deref(),
        Some("fix the build"),
        "a two-character escape is dropped whole"
    );
}

#[test]
fn test_prompt_caps_length() {
    let long = "x".repeat(5000);
    let got = prompt_intent(&long).unwrap();
    assert_eq!(got.chars().count(), 200);
    let cjk = "汉".repeat(500);
    let got = prompt_intent(&cjk).unwrap();
    assert_eq!(got.chars().count(), 200, "cap counts characters");
    let emoji = format!("start{}", "\u{1f680}".repeat(500));
    let got = prompt_intent(&emoji).unwrap();
    assert_eq!(got.chars().count(), 200);
    assert_eq!(
        got.chars().next_back(),
        Some('\u{1f680}'),
        "a four-byte scalar at the cut stays whole"
    );
}

#[test]
fn test_apply_records_first() {
    let mut d = fresh();
    apply_prompt(&mut d, record("first task"));
    apply_prompt(&mut d, record("second task"));
    assert_eq!(d.first_prompt.as_ref().unwrap().text, "first task");
}

#[test]
fn test_apply_advances_last() {
    let mut d = fresh();
    apply_prompt(&mut d, record("first task"));
    apply_prompt(&mut d, record("second task"));
    assert_eq!(d.last_prompt.as_ref().unwrap().text, "second task");
}

/// Prompt writes reach the store from separate blocking tasks, so a later
/// prompt can finish before an earlier one. Each slot follows the event id of
/// the record it holds, so the landing order does not decide the subject.
#[test]
fn test_apply_keeps_both_ends() {
    let mut d = fresh();
    let older = record("older task");
    let newer = record("newer task");
    apply_prompt(&mut d, newer.clone());
    apply_prompt(&mut d, older.clone());
    assert_eq!(
        d.first_prompt.as_ref().unwrap().text,
        "older task",
        "the earlier statement claims the first slot even when it lands late"
    );
    assert_eq!(
        d.last_prompt.as_ref().unwrap().text,
        "newer task",
        "the later statement keeps the last slot even when it lands first"
    );
    apply_prompt(&mut d, newer);
    assert_eq!(d.first_prompt.as_ref().unwrap().text, "older task");
}

#[test]
fn test_unread_log_skips_first() {
    let mut d = fresh();
    d.prompt_coverage = PromptCoverage::Unknown;
    apply_prompt(&mut d, record("a later task"));
    assert!(d.first_prompt.is_none(), "an unread log has no known first");
    assert_eq!(d.last_prompt.as_ref().unwrap().text, "a later task");
}

#[test]
fn test_title_prefers_user_name() {
    let mut d = fresh();
    d.name = Some("login rework".into());
    d.name_source = NameSource::User;
    apply_prompt(&mut d, record("fix the flaky login test"));
    let title = display_title(&d).expect("a named session has a title");
    assert_eq!(title.text, "login rework");
    assert_eq!(title.source, TitleSource::User);
}

#[test]
fn test_title_uses_last_prompt() {
    let mut d = fresh();
    apply_prompt(&mut d, record("first task"));
    apply_prompt(&mut d, record("second task"));
    let title = display_title(&d).expect("a stated task is a title");
    assert_eq!(
        title.text, "second task",
        "the newest statement names the work"
    );
    assert_eq!(title.source, TitleSource::LastPrompt);
}

#[test]
fn test_title_uses_first_prompt() {
    let mut d = fresh();
    d.first_prompt = Some(record("opening task"));
    let title = display_title(&d).expect("a lone first prompt is a title");
    assert_eq!(title.text, "opening task");
    assert_eq!(title.source, TitleSource::FirstPrompt);
}

/// A name no user chose is the opening subject at best, while the prompts say
/// what the work is now, so the name loses.
#[test]
fn test_title_ignores_auto_name() {
    let mut d = fresh();
    d.name = Some("stale derived name".into());
    apply_prompt(&mut d, record("real task"));
    let title = display_title(&d).expect("the prompt is the title");
    assert_eq!(title.text, "real task");
    assert_eq!(title.source, TitleSource::LastPrompt);
}

#[test]
fn test_title_absent() {
    assert!(display_title(&fresh()).is_none());
}

#[test]
fn test_partial_records_last() {
    let mut d = fresh();
    d.prompt_coverage = PromptCoverage::Partial;
    apply_prompt(&mut d, record("a later task"));
    assert!(d.first_prompt.is_none());
    assert_eq!(d.last_prompt.as_ref().unwrap().text, "a later task");
}

/// A descriptor written before prompts were recorded gains both slots from a
/// scan, and says afterwards that only part of the log was seen.
#[test]
fn test_backfill_records_both() {
    let mut d = fresh();
    d.prompt_coverage = PromptCoverage::Unknown;
    let first = record("opening task");
    let last = record("current task");
    apply_prompt_backfill(
        &mut d,
        Some(first.clone()),
        Some(last.clone()),
        PromptCoverage::Partial,
    );
    assert_eq!(d.first_prompt, Some(first));
    assert_eq!(d.last_prompt, Some(last));
    assert_eq!(d.prompt_coverage, PromptCoverage::Partial);
}

/// A live write that already recorded a newer prompt outranks an older scan,
/// so a background pass cannot move the session's subject backwards.
#[test]
fn test_backfill_keeps_newest() {
    let mut d = fresh();
    let older = record("older task");
    let newest = record("newest task");
    d.last_prompt = Some(newest.clone());
    apply_prompt_backfill(&mut d, None, Some(older), PromptCoverage::Partial);
    assert_eq!(d.last_prompt, Some(newest));
}

#[test]
fn test_backfill_takes_earliest() {
    let mut d = fresh();
    d.prompt_coverage = PromptCoverage::Unknown;
    let earlier = record("opening task");
    d.first_prompt = Some(record("second task"));
    apply_prompt_backfill(&mut d, Some(earlier.clone()), None, PromptCoverage::Partial);
    assert_eq!(d.first_prompt, Some(earlier));
}

/// A record that observed every event keeps that claim; a head-and-tail scan
/// cannot speak for the middle it never read. The reverse case raises a
/// record that never saw the whole log once a scan does.
#[test]
fn test_backfill_keeps_coverage() {
    let mut complete = fresh();
    apply_prompt_backfill(&mut complete, None, None, PromptCoverage::Partial);
    assert_eq!(complete.prompt_coverage, PromptCoverage::Complete);

    let mut unknown = fresh();
    unknown.prompt_coverage = PromptCoverage::Unknown;
    apply_prompt_backfill(&mut unknown, None, None, PromptCoverage::Complete);
    assert_eq!(unknown.prompt_coverage, PromptCoverage::Complete);
}

#[test]
fn test_backfill_leaves_name() {
    let mut d = fresh();
    d.name = Some("my session".into());
    d.name_source = NameSource::User;
    apply_prompt_backfill(&mut d, Some(record("task")), None, PromptCoverage::Partial);
    assert_eq!(d.name.as_deref(), Some("my session"));
    assert_eq!(d.name_source, NameSource::User);
}
