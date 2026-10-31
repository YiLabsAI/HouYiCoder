//! Tests for stripping half-written tool-call tags on interrupted child runs.

use super::*;
use houyicoder_context::EventId;

/// One assistant row carrying text, the shape an interrupted child leaves in
/// its log.
fn assistant_row(text: &str) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::AssistantMessage {
            text: text.to_string(),
            thinking: None,
        },
    }
}

/// A run stopped mid-stream leaves the tool-call marker the model had started
/// writing. Reading that back as the delegation's answer shows the user
/// protocol syntax instead of prose, which is what made the interrupted view
/// look broken.
#[test]
fn test_partial_answer_drops_tag() {
    let events = vec![assistant_row(
        "I checked the parser and it needs one more case.\n</|DSML|parameter></|DSML|invoke>",
    )];
    let got = extract_last_assistant(&events).expect("a partial answer");
    assert_eq!(
        got, "I checked the parser and it needs one more case.",
        "the half-written marker is not part of the answer"
    );
}

/// The same marker with a close after it is a complete tag pair the model meant
/// to write, so it stays: stripping it would delete part of a real answer.
#[test]
fn test_complete_tag_survives() {
    let text = "the row reads <|DSML|invoke|> as a marker";
    let events = vec![assistant_row(text)];
    assert_eq!(
        extract_last_assistant(&events).as_deref(),
        Some(text),
        "a closed marker is content"
    );
}

/// Text with no marker is returned untouched, including its trailing newline
/// handling: the stripping path must not reformat an ordinary answer.
#[test]
fn test_plain_answer_unchanged() {
    let text = "all checks pass";
    let events = vec![assistant_row(text)];
    assert_eq!(extract_last_assistant(&events).as_deref(), Some(text));
}
