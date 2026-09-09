//! Queue operations for user input submitted during an active run.

use std::collections::VecDeque;
use std::sync::Mutex;

use houyicoder_protocol::frontend::{PendingInputId, QueuedInput};

use super::{RunError, RunOutcome, RunResult, Runner};

/// Mid-turn user inputs awaiting durable commitment.
pub(crate) struct InputQueue {
    pending: Mutex<VecDeque<QueuedInput>>,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
        }
    }
}

impl Runner {
    /// Enqueue a user message for mid-turn injection. Callable from any
    /// Arc<Runner> the host holds, including a reconnecting serve.
    pub fn enqueue_input(&self, input: QueuedInput) {
        self.input_queue
            .pending
            .lock()
            .expect("input_queue pending lock")
            .push_back(input);
    }

    /// Enqueue a child-completion notification. Drained only after the
    /// user queue is empty, so user input never starves. The child id is
    /// carried so the durable event records which child finished.
    pub fn enqueue_notification(&self, child_session_id: String, text: String) {
        self.queued_notifications
            .lock()
            .expect("queued_notifications lock")
            .push_back((child_session_id, text));
    }

    /// Remove one queued message by stable identity. No-op when that
    /// entry was already drained, even if equal text was re-enqueued.
    pub fn remove_input(&self, id: PendingInputId) {
        let mut q = self
            .input_queue
            .pending
            .lock()
            .expect("input_queue pending lock");
        if let Some(pos) = q.iter().position(|input| input.id == id) {
            q.remove(pos);
        }
    }

    /// Remove the first matching text from a legacy queue notification.
    pub fn remove_input_by_text(&self, text: &str) {
        let mut queue = self
            .input_queue
            .pending
            .lock()
            .expect("input_queue pending lock");
        if let Some(index) = queue.iter().position(|input| input.text == text) {
            queue.remove(index);
        }
    }

    /// Drop every queued message without running it. A state-changing
    /// command (reset) or interrupted run invalidates the buffer, so
    /// orphaned texts must not leak into the next run.
    pub fn clear_input_queue(&self) {
        self.input_queue
            .pending
            .lock()
            .expect("input_queue pending lock")
            .clear();
    }

    /// Drop every queued notification. Not called on a normal terminal run
    /// (finalize_input_buffer): a pending notification survives to the next
    /// run so the parent still learns the child finished.
    pub fn clear_notifications(&self) {
        self.queued_notifications
            .lock()
            .expect("queued_notifications lock")
            .clear();
    }

    /// Drop the injection buffer on a terminal run end; keep it on
    /// Interruption (the run resumes). Called from the drive_loop wrapper.
    pub fn finalize_input_buffer(&self, result: &Result<RunResult, RunError>) {
        let terminal = match result {
            Ok(r) => !matches!(r.outcome, RunOutcome::Interruption(_)),
            Err(_) => true,
        };
        if terminal {
            self.clear_input_queue();
        }
    }

    /// Drain the pending user-input queue in FIFO order at a turn boundary.
    pub(crate) fn drain_pending_input(&self) -> Vec<QueuedInput> {
        self.input_queue
            .pending
            .lock()
            .expect("input_queue pending lock")
            .drain(..)
            .collect()
    }

    /// Test-only snapshot of the queued texts in FIFO order.
    #[cfg(test)]
    pub(crate) fn queued_input_snapshot(&self) -> Vec<String> {
        self.input_queue
            .pending
            .lock()
            .expect("input_queue pending lock")
            .iter()
            .map(|input| input.text.clone())
            .collect()
    }

    /// Test-only snapshot of the queued notification texts in FIFO order.
    pub fn queued_notifications_snapshot(&self) -> Vec<String> {
        self.queued_notifications
            .lock()
            .expect("queued_notifications lock")
            .iter()
            .map(|(_, text)| text.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::ToolRegistry;
    use super::super::runner_config::RunnerConfig;
    use super::{QueuedInput, Runner};
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_session::SessionStore;

    fn bare_runner() -> Runner {
        let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        Runner::new(
            store,
            std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("ok")),
            ToolRegistry::new(),
            RunnerConfig {
                model: "test".into(),
                instructions: "test".into(),
                max_turns: 5,
                max_output_tokens: 8_000,
                ..RunnerConfig::default()
            },
        )
    }

    /// Removing one duplicate targets its identity and leaves the other copy.
    #[test]
    fn test_remove_exact_id() {
        let r = bare_runner();
        let first = QueuedInput::new("same");
        let second = QueuedInput::new("same");
        r.enqueue_input(first.clone());
        r.enqueue_input(second.clone());
        r.remove_input(second.id);
        assert_eq!(r.queued_input_snapshot(), vec![first.text]);
    }
}
