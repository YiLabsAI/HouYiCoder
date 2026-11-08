//! Queue operations for user input submitted during an active run.

use std::collections::VecDeque;
use std::sync::Mutex;

use houyicoder_context::{NotificationTopic, SessionId};
use houyicoder_protocol::frontend::{PendingInputId, QueuedInput};
use serde_json::Value;

use crate::agent::{RunError, RunOutcome, RunResult, Runner};

/// One notification waiting for a turn boundary: what produced it, why it
/// exists, and the text the model reads. The reason travels with the text so
/// the assembly frames each notification for what it is.
pub(crate) struct QueuedNotification {
    pub(crate) source_id: String,
    pub(crate) topic: NotificationTopic,
    pub(crate) text: String,
    /// The stored raw output the text names, when the producer kept any. The
    /// durable record carries it, and the reclaim pass reads the address from
    /// there; a block no record names is released.
    pub(crate) capture: Option<Value>,
}

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

    /// Enqueue a background result — a finished subagent or a finished
    /// command — for the next turn boundary. Drained only after the user queue
    /// is empty, so user input never starves. The source id is carried so the
    /// durable event records what produced it.
    pub fn enqueue_notification(
        &self,
        source_id: String,
        topic: NotificationTopic,
        text: String,
        capture: Option<Value>,
    ) {
        self.queued_notifications
            .lock()
            .expect("queued_notifications lock")
            .push_back(QueuedNotification {
                source_id,
                topic,
                text,
                capture,
            });
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

    /// How many notifications currently wait for a turn boundary to consume
    /// them. The drain task reports this to the frontend as the wake signal;
    /// the count is read after the enqueue in the same lock domain, so a
    /// client that wakes consumes at least the reported figure.
    pub fn queued_notification_count(&self) -> u32 {
        self.queued_notifications
            .lock()
            .expect("queued_notifications lock")
            .len() as u32
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
            .map(|notification| notification.text.clone())
            .collect()
    }

    /// Test-only snapshot of the topics of queued notifications, in FIFO
    /// order, so a test can tell a task ending from a child completion.
    pub fn queued_notification_topics_snapshot(&self) -> Vec<NotificationTopic> {
        self.queued_notifications
            .lock()
            .expect("queued_notifications lock")
            .iter()
            .map(|notification| notification.topic)
            .collect()
    }

    /// Test-only snapshot of the stored output addresses queued beside those
    /// texts, in the same order.
    pub fn queued_notification_captures_snapshot(&self) -> Vec<Option<Value>> {
        self.queued_notifications
            .lock()
            .expect("queued_notifications lock")
            .iter()
            .map(|notification| notification.capture.clone())
            .collect()
    }

    /// Drain the turn-boundary injection buffer: host-submitted user
    /// interjections, bus-inbox steering texts, and lower-priority completion
    /// notifications. Appends each as a user message the next model call sees,
    /// then re-runs memory recall + skill listing for the injected query.
    /// Notifications defer to user input — drained only when the user queue is
    /// empty — so a notification never jumps ahead of a pending user message.
    pub(crate) async fn drain_turn_boundary(&self, session: SessionId) -> Result<bool, RunError> {
        let pending_user: Vec<QueuedInput> = self.drain_pending_input();
        let inbox_pending = self.drain_inbox();
        let pending_notifications: Vec<QueuedNotification> = if pending_user.is_empty() {
            let mut q = self
                .queued_notifications
                .lock()
                .expect("queued_notifications lock");
            q.drain(..).collect()
        } else {
            Vec::new()
        };
        let had_pending = !pending_user.is_empty()
            || !inbox_pending.is_empty()
            || !pending_notifications.is_empty();
        for input in pending_user {
            self.append_mid_turn_input(session, input.text, Some(input.id.0))
                .await?;
        }
        for msg in inbox_pending {
            self.append_mid_turn_input(session, msg, None).await?;
        }
        for notification in pending_notifications {
            self.append_notification(
                session,
                notification.source_id,
                notification.topic,
                notification.text,
                notification.capture,
            )
            .await?;
        }
        if had_pending {
            self.memory.recall(session).await?;
            self.inject_skill_listing_and_body(session).await?;
        }
        Ok(had_pending)
    }
}

#[cfg(test)]
mod tests;
