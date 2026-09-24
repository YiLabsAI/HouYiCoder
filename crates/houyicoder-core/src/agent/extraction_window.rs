//! The evidence boundary one background extraction pass may read.

use houyicoder_api::session::last_user_input_id;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};

/// The durable history one extraction pass may read, cut at query boundaries
/// so tool calls stay beside their results.
pub(crate) struct ExactExtractionWindow<'a> {
    session: SessionId,
    trigger_user_event: EventId,
    start_event: EventId,
    end_event: EventId,
    events: &'a [SessionLogEntry],
}

impl<'a> ExactExtractionWindow<'a> {
    pub(crate) fn session(&self) -> SessionId {
        self.session
    }

    pub(crate) fn trigger_user_event(&self) -> EventId {
        self.trigger_user_event
    }

    pub(crate) fn start_event(&self) -> EventId {
        self.start_event
    }

    pub(crate) fn end_event(&self) -> EventId {
        self.end_event
    }

    pub(crate) fn events(&self) -> &'a [SessionLogEntry] {
        self.events
    }

    /// How many model-visible messages the window holds. A metric only: the
    /// events are the boundary, the count is not.
    pub(crate) fn model_visible_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| is_model_visible(&e.event))
            .count()
    }

    /// The range an earlier pass left unconsumed. A snapshot with no cursor
    /// is a fresh session, so the whole log is unconsumed. None when the
    /// cursor names an event this snapshot does not hold.
    pub(crate) fn unconsumed(
        events: &'a [SessionLogEntry],
        cursor: Option<&EventId>,
    ) -> Option<&'a [SessionLogEntry]> {
        let start = match cursor {
            None => 0,
            Some(id) => match events.iter().position(|e| &e.id == id) {
                Some(index) => index + 1,
                None => return None,
            },
        };
        Some(&events[start.min(events.len())..])
    }

    /// Cut the whole query turns out of an unconsumed range. The window
    /// begins on a user input so every tool call sits beside its result.
    /// None when the range holds no complete query turn.
    pub(crate) fn from_unconsumed(tail: &'a [SessionLogEntry]) -> Option<Self> {
        let offset = tail
            .iter()
            .position(|e| matches!(e.event, SessionEvent::UserInput { .. }))?;
        let events = &tail[offset..];
        let trigger_user_event =
            last_user_input_id(events).expect("a window starting on a user input has at least one");
        let last = events
            .last()
            .expect("a window with a user input is non-empty");
        Some(Self {
            session: events[0].session,
            trigger_user_event,
            start_event: events[0].id,
            end_event: last.id,
            events,
        })
    }
}

/// Whether a durable event is one the model reads.
fn is_model_visible(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::UserInput { .. }
            | SessionEvent::MidTurnInput { .. }
            | SessionEvent::AssistantMessage { .. }
    )
}

#[cfg(test)]
#[path = "extraction_window_tests.rs"]
mod tests;
