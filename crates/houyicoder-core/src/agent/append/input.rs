//! How a user's words enter the log: a fresh instruction, or an interjection
//! queued while a run was active and appended at the next turn boundary.
//!
//! Both writers record the task statement on the session descriptor after the
//! durable append succeeded, so the recorded event id names an event that
//! exists. Input withdrawn before it reached the log never passes here, which
//! is why a cancelled interjection cannot become a session's subject.

use houyicoder_context::session_intent::prompt_intent;
use houyicoder_context::{EventId, SessionEvent, SessionId};

use super::{RunError, Runner, new_event};
use crate::agent::multi_agent::bus_types::BusMessage;

impl Runner {
    /// Append a user input event to the log.
    pub(crate) async fn append_user_input(
        &self,
        session: SessionId,
        text: String,
    ) -> Result<(), RunError> {
        let prompt = prompt_intent(&text);
        let entry = new_event(session, SessionEvent::UserInput { text });
        let event_id = entry.id;
        self.store.append(entry).await?;
        self.record_session_intent(session, prompt, event_id);
        Ok(())
    }

    /// Hand the statement a durable input carried to the recorder, when the
    /// runner has one. The append already succeeded, so the event id names an
    /// event that exists.
    fn record_session_intent(&self, session: SessionId, prompt: Option<String>, event_id: EventId) {
        if let Some(recorder) = self.prompt_recorder.as_ref() {
            recorder.record(session, prompt, event_id);
        }
    }
    /// Drain the bus inbox receiver (a spawned child's steering channel)
    /// into the texts the drive loop appends as user messages this turn.
    /// Empty for the top-level runner, which has no inbox. Non-blocking.
    pub(crate) fn drain_inbox(&self) -> Vec<String> {
        let mut guard = self.inbox.lock().expect("inbox lock");
        let Some(rx) = guard.as_mut() else {
            return Vec::new();
        };
        let mut acc = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let BusMessage::Inbox { text } = msg {
                acc.push(text);
            }
        }
        acc
    }

    /// Append a queued interjection: a user message the human queued while a
    /// run was active, drained + appended at the next turn boundary. The
    /// model input assembly wraps it with a framing note so the model reads
    /// it as a mid-work interjection (continue the task + address), not a
    /// fresh instruction that drops the task already underway. The durable
    /// text is the bare user input (the transcript shows it as sent).
    pub(crate) async fn append_mid_turn_input(
        &self,
        session: SessionId,
        text: String,
        pending_input_id: Option<u64>,
    ) -> Result<(), RunError> {
        let prompt = prompt_intent(&text);
        let entry = new_event(
            session,
            SessionEvent::MidTurnInput {
                text,
                pending_input_id,
            },
        );
        let event_id = entry.id;
        self.store.append(entry).await?;
        self.record_session_intent(session, prompt, event_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
