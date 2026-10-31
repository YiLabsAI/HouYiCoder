//! Dropping incomplete tool-call tags left in assistant messages when a run is interrupted.

use houyicoder_context::{SessionEvent, SessionLogEntry};

/// Walk the child's rows backwards for the last assistant message carrying
/// text; the partial result a non-final terminal leaves behind. None when no
/// assistant message (or none with text) was emitted.
pub(crate) fn extract_last_assistant(events: &[SessionLogEntry]) -> Option<String> {
    for ev in events.iter().rev() {
        if let SessionEvent::AssistantMessage { ref text, .. } = ev.event
            && !text.is_empty()
        {
            let cleaned = strip_unclosed_tag(text);
            return Some(if cleaned.trim().is_empty() {
                text.clone()
            } else {
                cleaned
            });
        }
    }
    None
}

/// Drop a tool-call tag the stream left half-written.
///
/// A model that emits its tool call as text writes an opening marker and then
/// the call itself. Stopping the run mid-stream leaves that opening marker and
/// the fragments after it in the assistant text, and reading it back gives the
/// user a line of protocol syntax where prose belongs. Only the unclosed tail
/// is removed: a marker that a matching close follows is text the model meant
/// to say, and cutting it would silently delete part of a real answer.
pub(crate) fn strip_unclosed_tag(text: &str) -> String {
    const OPENERS: [&str; 2] = ["</|", "<|"];
    let Some(start) = OPENERS.iter().filter_map(|o| text.find(o)).min() else {
        return text.to_string();
    };
    // A marker followed by a close is a complete tag pair, which is content.
    let tail = &text[start..];
    if tail.contains("|>") {
        return text.to_string();
    }
    text[..start].trim_end().to_string()
}
