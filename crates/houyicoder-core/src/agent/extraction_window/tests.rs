use super::*;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};

fn entry(session: SessionId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event,
    }
}

fn user(session: SessionId, text: &str) -> SessionLogEntry {
    entry(
        session,
        SessionEvent::UserInput {
            text: text.to_string(),
        },
    )
}

fn assistant(session: SessionId, text: &str) -> SessionLogEntry {
    entry(
        session,
        SessionEvent::AssistantMessage {
            text: text.to_string(),
            thinking: None,
        },
    )
}

fn tool_call(session: SessionId, call_id: &str) -> SessionLogEntry {
    entry(
        session,
        SessionEvent::ToolCall {
            call_id: call_id.to_string(),
            tool: "save_memory".to_string(),
            input: serde_json::json!({}),
        },
    )
}

fn tool_result(session: SessionId, call_id: &str) -> SessionLogEntry {
    entry(
        session,
        SessionEvent::ToolResult {
            call_id: call_id.to_string(),
            output: serde_json::json!({"saved": call_id}),
            duration_ms: 0,
        },
    )
}

fn sid() -> SessionId {
    SessionId::new()
}

/// A snapshot with no cursor is a fresh session, so the whole log is
/// unconsumed and the window reaches the first turn.
#[test]
fn test_fresh_reads_whole_log() {
    let session = sid();
    let first = user(session, "first ask");
    let last = assistant(session, "first answer");
    let start = first.id;
    let end = last.id;
    let events = vec![first, last];
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&events, None).expect("fresh cursor locates a range"),
    )
    .expect("a turn is present");
    assert_eq!(window.events().len(), 2);
    assert_eq!(window.start_event(), start);
    assert_eq!(window.end_event(), end);
}

/// A cursor at the end of an earlier turn excludes that turn from the window.
#[test]
fn test_consumed_turn_excluded() {
    let session = sid();
    let first = user(session, "first ask");
    let first_answer = assistant(session, "first answer");
    let second = user(session, "second ask");
    let second_answer = assistant(session, "second answer");
    let cursor = first_answer.id;
    let second_id = second.id;
    let second_answer_id = second_answer.id;
    let events = vec![first, first_answer, second, second_answer];
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&events, Some(&cursor)).expect("cursor locates a range"),
    )
    .expect("a turn follows");
    assert_eq!(
        window.events().len(),
        2,
        "only the turn after the cursor is read"
    );
    assert_eq!(window.start_event(), second_id);
    assert_eq!(window.end_event(), second_answer_id);
}

/// A cursor that lands mid-turn leaves a dangling tool pair behind; the
/// window drops it and starts on the next user input so no tool call loses
/// its result across the cut.
#[test]
fn test_dangling_pair_dropped() {
    let session = sid();
    let ask = user(session, "ask");
    let partial = assistant(session, "calling a tool");
    let call = tool_call(session, "c1");
    let result = tool_result(session, "c1");
    let next = user(session, "next ask");
    let final_answer = assistant(session, "next answer");
    let cursor = partial.id;
    let next_id = next.id;
    let events = vec![ask, partial, call, result, next, final_answer];
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&events, Some(&cursor)).expect("cursor locates a range"),
    )
    .expect("a turn follows");
    assert_eq!(window.events().len(), 2);
    assert_eq!(window.start_event(), next_id);
    assert!(
        !window
            .events()
            .iter()
            .any(|e| matches!(e.event, SessionEvent::ToolCall { .. })),
        "the dangling pair is not in the window"
    );
}

/// A cursor that names an event the snapshot does not hold cannot locate the
/// consumed prefix, so no range is returned.
#[test]
fn test_lost_cursor_returns_none() {
    let session = sid();
    let events = vec![user(session, "ask"), assistant(session, "answer")];
    let foreign = EventId::new();
    assert!(
        ExactExtractionWindow::unconsumed(&events, Some(&foreign)).is_none(),
        "a cursor outside the snapshot locates nothing"
    );
}

/// An empty unconsumed range holds no complete query turn.
#[test]
fn test_empty_tail_returns_none() {
    let session = sid();
    let last = assistant(session, "answer");
    let events = vec![user(session, "ask"), last];
    let tail =
        ExactExtractionWindow::unconsumed(&events, Some(&events[1].id)).expect("cursor at tail");
    assert!(tail.is_empty());
    assert!(ExactExtractionWindow::from_unconsumed(tail).is_none());
}

/// A tail that holds only non-turn events has no query turn to read.
#[test]
fn test_tail_without_turn_none() {
    let session = sid();
    let call = tool_call(session, "c1");
    let result = tool_result(session, "c1");
    let events = vec![
        user(session, "ask"),
        assistant(session, "answer"),
        call,
        result,
    ];
    let cursor = events[1].id;
    let tail = ExactExtractionWindow::unconsumed(&events, Some(&cursor)).expect("cursor locates");
    assert!(ExactExtractionWindow::from_unconsumed(tail).is_none());
}

/// The trigger is the newest user input in the window, the turn whose
/// completion fired this pass.
#[test]
fn test_trigger_is_newest_turn() {
    let session = sid();
    let first = user(session, "first ask");
    let second = user(session, "second ask");
    let second_id = second.id;
    let events = vec![
        first,
        assistant(session, "first answer"),
        second,
        assistant(session, "second answer"),
    ];
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&events, None).expect("fresh cursor locates"),
    )
    .expect("turns present");
    assert_eq!(window.trigger_user_event(), second_id);
}

/// Tool calls and their results stay together inside the window so the
/// forked agent reads each pair as a unit.
#[test]
fn test_tool_pairs_kept_intact() {
    let session = sid();
    let ask = user(session, "ask");
    let call = tool_call(session, "c1");
    let result = tool_result(session, "c1");
    let answer = assistant(session, "answer");
    let events = vec![ask, call, result, answer];
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&events, None).expect("fresh cursor locates"),
    )
    .expect("a turn is present");
    let has_call = window.events().iter().any(
        |e| matches!(&e.event, SessionEvent::ToolCall { call_id, .. } if call_id.as_str() == "c1"),
    );
    let has_result = window.events().iter().any(|e| {
        matches!(&e.event, SessionEvent::ToolResult { call_id, .. } if call_id.as_str() == "c1")
    });
    assert!(has_call && has_result, "the pair stays together");
}

/// The visible count covers user, mid-turn, and assistant events but skips
/// tool calls and results, which are scaffolding the model already sees.
#[test]
fn test_visible_count_skips_tools() {
    let session = sid();
    let events = vec![
        user(session, "ask"),
        tool_call(session, "c1"),
        tool_result(session, "c1"),
        assistant(session, "answer"),
    ];
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&events, None).expect("fresh cursor locates"),
    )
    .expect("a turn is present");
    assert_eq!(window.model_visible_count(), 2);
}

/// A window spanning several turns starts at the first user input and ends
/// at the last event, so a coalesced pass reads every turn it covers.
#[test]
fn test_range_spans_window_events() {
    let session = sid();
    let first = user(session, "first ask");
    let last = assistant(session, "second answer");
    let start = first.id;
    let end = last.id;
    let events = vec![
        first,
        assistant(session, "first answer"),
        user(session, "second ask"),
        last,
    ];
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&events, None).expect("fresh cursor locates"),
    )
    .expect("turns present");
    assert_eq!(window.start_event(), start);
    assert_eq!(window.end_event(), end);
    assert_eq!(window.events().len(), 4);
}
