//! Tests for this module.

use super::*;
use houyicoder_context::{EventId, SessionId};

fn evt(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event,
    }
}

fn user(text: &str) -> SessionLogEntry {
    evt(SessionEvent::UserInput { text: text.into() })
}

fn assistant(text: &str) -> SessionLogEntry {
    evt(SessionEvent::AssistantMessage {
        text: text.into(),
        thinking: None,
    })
}

fn tool_call(call_id: &str) -> SessionLogEntry {
    evt(SessionEvent::ToolCall {
        call_id: call_id.into(),
        tool: "echo".into(),
        input: serde_json::json!({ "call": call_id }),
    })
}

fn note() -> SessionLogEntry {
    evt(SessionEvent::MidTurnInput {
        text: "keep going".into(),
        pending_input_id: None,
    })
}

fn text_of(item: &InputItem) -> &str {
    match item {
        InputItem::User { content } => content,
        InputItem::Assistant { content, .. } => content,
        InputItem::ToolResult { .. } => "",
    }
}

/// Every tool result handed to the model must answer a call the same window
/// carries. A result whose call was cut away is an invalid request.
fn assert_results_paired(items: &[InputItem]) {
    let calls: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            InputItem::Assistant { tool_calls, .. } => Some(tool_calls.iter()),
            _ => None,
        })
        .flatten()
        .map(|call| call.id.as_str())
        .collect();
    for item in items {
        if let InputItem::ToolResult { call_id, .. } = item {
            assert!(
                calls.contains(&call_id.as_str()),
                "result {call_id} has no call in the window"
            );
        }
    }
}

#[test]
fn test_short_session_kept_whole() {
    let events = vec![user("check the build"), assistant("the build is green")];
    let items = build_recap_input(&events, &Tokenizer::new(), 0).expect("fits");
    assert_eq!(items.len(), 2);
    assert_eq!(text_of(&items[0]), "check the build");
}

/// The window keeps exactly the newest messages it may send, so a long
/// session still reaches the model with the latest work in view.
#[test]
fn test_long_session_keeps_newest() {
    let mut events = Vec::new();
    for turn in 0..40 {
        events.push(user(&format!("request {turn}")));
        events.push(assistant(&format!("reply {turn}")));
    }
    let items = build_recap_input(&events, &Tokenizer::new(), 0).expect("fits");
    assert_eq!(items.len(), RECAP_MESSAGE_LIMIT);
    let last = items.last().expect("window is not empty");
    assert_eq!(text_of(last), "reply 39");
}

/// The message limit is not the only bound: a long session of large messages
/// must be cut by the token budget some turns before the message limit.
#[test]
fn test_token_budget_cuts_window() {
    let mut events = Vec::new();
    for turn in 0..12 {
        events.push(user(&format!("request {turn} {}", "x".repeat(8_000))));
        events.push(assistant(&format!("reply {turn} {}", "y".repeat(8_000))));
    }
    let tokenizer = Tokenizer::new();
    let items = build_recap_input(&events, &tokenizer, 0).expect("fits");
    assert!(
        items.len() < RECAP_MESSAGE_LIMIT,
        "the token budget chose the cut, not the message cap"
    );
    let used: u32 = items.iter().map(|item| tokenizer.count_input(item)).sum();
    assert!(used <= RECAP_INPUT_TOKENS);
    assert_eq!(
        text_of(items.last().expect("window is not empty")),
        format!("reply 11 {}", "y".repeat(8_000))
    );
    // Maximality: the window is the longest that fits, not merely the newest
    // message. One message further back must already break the budget.
    let first = text_of(&items[0]);
    let pos = events
        .iter()
        .position(|entry| match &entry.event {
            SessionEvent::UserInput { text } | SessionEvent::AssistantMessage { text, .. } => {
                text == first
            }
            _ => false,
        })
        .expect("the window's first item comes from the log");
    let wider: u32 = assemble_model_input(&events[pos.saturating_sub(1)..], None)
        .iter()
        .map(|item| tokenizer.count_input(item))
        .sum();
    assert!(
        wider > RECAP_INPUT_TOKENS,
        "one more message must exceed the budget, got {wider} tokens"
    );
}

/// A window that opened on a tool result would hand the model a result whose
/// call it cannot see. Each shape puts the newest work close enough to the cut
/// that a plain item count would land inside a tool group; every window must
/// still open a message that carries its calls.
#[test]
fn test_cut_pairs_tool_results() {
    let tokenizer = Tokenizer::new();
    for extra in 0..6 {
        let mut events = Vec::new();
        for turn in 0..14 {
            let calls = if turn == 13 { 1 + extra } else { 1 };
            events.push(user(&format!("request {turn}")));
            events.push(assistant(&format!("reply {turn}")));
            for call in 0..calls {
                let call_id = format!("call-{turn}-{call}");
                events.push(tool_call(&call_id));
                events.push(evt(SessionEvent::tool_result(
                    &call_id,
                    serde_json::json!({ "turn": turn }),
                )));
            }
        }
        let items = build_recap_input(&events, &tokenizer, 0).expect("fits");
        assert_results_paired(&items);
    }
}

fn interjections() -> [SessionLogEntry; 2] {
    [
        note(),
        evt(SessionEvent::NotificationInjected {
            child_session_id: "child-1".into(),
            turn: 1,
            order: 0,
            topic: "review".into(),
            summary: "the child finished".into(),
        }),
    ]
}

/// Background work may land between a call and its result. A window that
/// opened on it would keep the result and drop the call it answers, so the
/// interjection is not a place the window may start.
#[test]
fn test_interleaved_event_keeps_pairing() {
    let tokenizer = Tokenizer::new();
    for interjection in interjections() {
        let events = vec![
            user("start"),
            assistant("calling the tool"),
            tool_call("call-1"),
            interjection,
            evt(SessionEvent::tool_result(
                "call-1",
                serde_json::json!({ "ok": true }),
            )),
        ];
        let items = build_recap_input(&events, &tokenizer, 0).expect("fits");
        assert_results_paired(&items);
        assert_eq!(text_of(&items[0]), "start");
    }
}

/// A cut forced by the message limit must still land outside an open call
/// group, even when stepping back costs it the request that opened the group.
/// The group here is too large to send whole, so the window begins after it
/// rather than inside it.
#[test]
fn test_cut_respects_open_calls() {
    let tokenizer = Tokenizer::new();
    for interjection in interjections() {
        let mut events = vec![
            user("start"),
            assistant("calling the tools"),
            tool_call("call-1"),
            tool_call("call-2"),
        ];
        events.push(interjection);
        for call in 1..=2 {
            events.push(evt(SessionEvent::tool_result(
                format!("call-{call}"),
                serde_json::json!({ "ok": true }),
            )));
        }
        for turn in 0..14 {
            events.push(user(&format!("request {turn}")));
            events.push(assistant(&format!("reply {turn}")));
        }
        let items = build_recap_input(&events, &tokenizer, 0).expect("fits");
        assert_results_paired(&items);
        assert_eq!(
            text_of(&items[0]),
            "request 0",
            "the cut must begin after the whole group, not inside it: {:?}",
            items[0]
        );
    }
}

/// The user request and the reminders that follow it are one message. A cut
/// between them would open the summary on a reminder and lose the request the
/// user actually made.
#[test]
fn test_user_reminder_keeps_request() {
    let mut events = vec![
        user("fix the parser"),
        evt(SessionEvent::MemoryRecall {
            text: "memory: the parser lives in src/parse".into(),
            keys: vec!["parser".into()],
            bytes: 41,
        }),
    ];
    // The request sits exactly at the message-limit boundary: a window that
    // may start on the reminder would drop it and still fit.
    for turn in 0..15 {
        events.push(user(&format!("request {turn}")));
        events.push(assistant(&format!("reply {turn}")));
    }
    let items = build_recap_input(&events, &Tokenizer::new(), 0).expect("fits");
    assert!(
        text_of(&items[0]).starts_with("fix the parser"),
        "the window opens on the request, not the reminder: {:?}",
        items[0]
    );
    assert!(
        text_of(&items[0]).contains("memory: the parser lives in src/parse"),
        "the reminder stays with the request it followed: {:?}",
        items[0]
    );
}

/// The newest request alone can exceed what a recap may send. A truncated copy
/// of it would misdescribe the work, so the host sends nothing.
#[test]
fn test_oversized_message_skips() {
    let events = vec![user(&"x".repeat(40_000))];
    assert_eq!(
        build_recap_input(&events, &Tokenizer::new(), 0),
        Err(RecapWindowSkip::NewestTooLarge)
    );
}

/// The request pays for its instruction and memory block before the
/// conversation starts. When that reservation uses the whole budget, no
/// message fits and the host sends nothing.
#[test]
fn test_full_reserve_skips() {
    let events = vec![user("hello"), assistant("hi")];
    let tokenizer = Tokenizer::new();
    assert_eq!(
        build_recap_input(&events, &tokenizer, RECAP_INPUT_TOKENS),
        Err(RecapWindowSkip::ReserveExhausted)
    );
}

/// Work the user cleared stays out of the summary: describing a discarded
/// direction as current work would mislead on return. The last clear wins.
#[test]
fn test_context_clear_truncates() {
    let events = vec![
        user("first direction"),
        assistant("first reply"),
        evt(SessionEvent::ContextCleared { prior_turn: 1 }),
        user("second direction"),
        assistant("second reply"),
        evt(SessionEvent::ContextCleared { prior_turn: 2 }),
        user("current direction"),
        assistant("current reply"),
    ];
    let items = build_recap_input(&events, &Tokenizer::new(), 0).expect("fits");
    assert_eq!(items.len(), 2);
    assert_eq!(text_of(&items[0]), "current direction");
}

/// Control text the runner injects for its own flow is not something the user
/// wrote, so the recap model never reads it.
#[test]
fn test_control_text_stays_out() {
    let events = vec![
        user("real request"),
        assistant("ok"),
        evt(SessionEvent::MetaUser {
            text: "INTERNAL NUDGE: resume directly".into(),
        }),
    ];
    let items = build_recap_input(&events, &Tokenizer::new(), 0).expect("fits");
    assert!(
        items
            .iter()
            .all(|item| !text_of(item).contains("INTERNAL NUDGE"))
    );
    let only_control = vec![evt(SessionEvent::MetaUser {
        text: "INTERNAL NUDGE: resume directly".into(),
    })];
    assert_eq!(
        build_recap_input(&only_control, &Tokenizer::new(), 0),
        Err(RecapWindowSkip::Empty)
    );
}

#[test]
fn test_empty_log_skips() {
    assert_eq!(
        build_recap_input(&[], &Tokenizer::new(), 0),
        Err(RecapWindowSkip::Empty)
    );
}
