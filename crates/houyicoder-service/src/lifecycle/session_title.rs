//! The title a session is shown by when its descriptor records no prompt.
//!
//! A session opened before prompts were recorded on its descriptor carries
//! none, while its log head still says what the work was. The read here is
//! bounded and answers with the first input that states a task, so a listing
//! can title such a session without opening its whole log. It is a display
//! fallback only: nothing found here is written back to the record, so the
//! record's own account of what it carries stays as it was.

use houyicoder_api::session::SessionLog;
use houyicoder_context::SessionId;
use houyicoder_context::session_intent::prompt_intent;
use houyicoder_context::{SessionEvent, SessionLogEntry};

/// How much of a log's head is read to find its first task statement. A
/// record that opens with metadata or a greeting still has the user's words
/// within this 64 KiB window, and the read stays small enough for a listing
/// pass.
const LOG_HEAD_BYTES: u64 = 64 * 1024;

/// The first user input in a session's log head that states the work.
///
/// None when the log is missing, its head holds no parseable record, or no
/// input in the window states a task. The opening turn and the interjection
/// a user queued both count. An interjection without a queue identity is
/// skipped, because that shape marks a message another agent sent and, on
/// records written before the field existed, an interjection whose author
/// cannot be told apart from one. Each candidate is judged by the rule the
/// descriptor recorder applies, so a greeting does not become a title here.
pub fn log_head_title(log: &dyn SessionLog, session: SessionId) -> Option<String> {
    let read = log.backend().read_log_range(session, 0, LOG_HEAD_BYTES);
    for (_, line) in &read.lines {
        let Ok(entry) = serde_json::from_str::<SessionLogEntry>(line) else {
            continue;
        };
        let text = match &entry.event {
            SessionEvent::UserInput { text } => text,
            SessionEvent::MidTurnInput {
                text,
                pending_input_id: Some(_),
            } => text,
            _ => continue,
        };
        if let Some(title) = prompt_intent(text) {
            return Some(title);
        }
    }
    None
}

#[cfg(test)]
mod tests;
