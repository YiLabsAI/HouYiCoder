//! Whether a durable tool call is a user-typed shell command.
//!
//! A command typed at the input box runs through the same bash tool a model
//! call runs through, so the log holds an ordinary ToolCall and the ToolResult
//! that answers it. The transcript still tells the two apart: the user's own
//! command reads as a command line, the model's call as a tool step. The event
//! carries no origin field, so the origin rides the call id the host writes.

use std::collections::HashSet;

/// The prefix the host puts on the call id of a user-typed shell command.
const USER_SHELL_CALL_PREFIX: &str = "shell-";

/// The length of an event id in its display spelling. A user shell call id is
/// the prefix followed by exactly this many characters.
const EVENT_ID_DISPLAY_LEN: usize = 26;

/// The call id for a user-typed shell command, from the event id it is
/// generated beside.
///
/// The id orders the session log, so it stays unique across restarts where a
/// per-process counter would not. Generation and recognition live in one
/// module, so a change to either breaks the round-trip test beside this
/// function rather than silently separating the writer from its readers.
pub fn user_shell_call_id(event_id: &str) -> String {
    format!("{USER_SHELL_CALL_PREFIX}{event_id}")
}

/// The id a model-issued call keeps so it can never read as a user shell
/// command.
///
/// A provider chooses its call ids, and one that happens to spell the reserved
/// shape would have its call rendered as a command line the user typed,
/// excluded from the turn summary, and budgeted as user shell history. The
/// host normalizes such an id once, before the call is appended, so the log,
/// the results that answer it, and the transcript all carry the same distinct
/// id.
pub fn model_call_id(call_id: &str) -> String {
    if is_user_shell_call_id(call_id) {
        format!("model-{call_id}")
    } else {
        call_id.to_string()
    }
}

/// The id one model-issued call keeps in a response where every id must stay
/// distinct.
///
/// Renaming a reserved-shaped id can land on another id the provider sent
/// (a call spelled model- followed by that same reserved shape), and two calls
/// sharing one id would route a result to the wrong call in every consumer
/// that keys by id. The taken set is the ids already assigned in this
/// response; a collision takes a numeric suffix, which also keeps the result
/// out of the reserved shape.
pub fn unique_model_call_id(call_id: &str, taken: &mut HashSet<String>) -> String {
    let base = model_call_id(call_id);
    let mut candidate = base.clone();
    let mut suffix = 1u32;
    while !taken.insert(candidate.clone()) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// Whether a call id names a user-typed shell command.
///
/// The check is the exact id shape rather than the prefix alone: a provider
/// call id is free-form text, and a false match would paint a model call as
/// the user's own command line. The suffix must be one event id in its display
/// spelling, which is what the call-id constructor produces.
pub fn is_user_shell_call_id(call_id: &str) -> bool {
    let Some(suffix) = call_id.strip_prefix(USER_SHELL_CALL_PREFIX) else {
        return false;
    };
    suffix.len() == EVENT_ID_DISPLAY_LEN && suffix.bytes().all(is_event_id_char)
}

/// Whether one character belongs to the alphabet an event id is spelled in.
fn is_event_id_char(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
}

#[cfg(test)]
mod tests;
