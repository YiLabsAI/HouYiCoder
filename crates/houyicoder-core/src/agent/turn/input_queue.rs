//! Queue operations for user input submitted during an active run.

use std::collections::VecDeque;
use std::sync::Mutex;

use houyicoder_api::agent_event::NotificationQueueEvent;
use houyicoder_context::{NotificationTopic, SessionEvent, SessionId};
use houyicoder_protocol::frontend::{PendingInputId, QueuedInput};
use serde_json::Value;

use crate::agent::{RunError, RunOutcome, RunResult, Runner, new_event};

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

/// The queued notifications plus the counter that stamps every transition.
///
/// The count is read under the queue lock but the event leaves after it, so
/// two transitions can reach a consumer out of order. Stamping each one under
/// the same lock lets the consumer drop a report older than what it applied.
#[derive(Default)]
pub(crate) struct NotificationQueue {
    pub(crate) items: VecDeque<QueuedNotification>,
    pub(crate) generation: u64,
}

impl NotificationQueue {
    /// Apply one transition, returning the stamp it owns and the depth left.
    fn transition(&mut self, change: impl FnOnce(&mut VecDeque<QueuedNotification>)) -> (u64, u32) {
        change(&mut self.items);
        self.generation = self.generation.wrapping_add(1);
        (self.generation, self.items.len() as u32)
    }
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
        let (generation, count) = {
            let mut q = self
                .queued_notifications
                .lock()
                .expect("queued_notifications lock");
            q.transition(|items| {
                items.push_back(QueuedNotification {
                    source_id,
                    topic,
                    text,
                    capture,
                });
            })
        };
        self.publish_notification_count(generation, count);
    }

    /// Emit the authoritative queued-notification level.
    ///
    /// Every transition of the queue goes through here with the depth and the
    /// transition stamp read under the same lock that produced them, so a
    /// consumer can discard a report it receives out of order.
    fn publish_notification_count(&self, generation: u64, count: u32) {
        self.events
            .emit_notification_queue(NotificationQueueEvent { count, generation });
    }

    /// Take the stamped level a reader attaching now should start from.
    ///
    /// A client that just connected knows nothing about the queue, and no
    /// later event would tell it about endings that arrived before it. The
    /// stamp is taken under the queue lock so the report orders against the
    /// producers': an older reading can never overwrite a newer one. An empty
    /// queue reports nothing, because every client already starts at zero.
    ///
    /// The caller delivers the pair; taking the stamp here and delivering it
    /// there keeps the report independent of whether this runner has an event
    /// handler installed.
    pub fn stamp_queued_notification_level(&self) -> Option<(u64, u32)> {
        let (generation, count) = {
            let mut q = self
                .queued_notifications
                .lock()
                .expect("queued_notifications lock");
            q.transition(|_| {})
        };
        (count > 0).then_some((generation, count))
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
        let (generation, count) = {
            let mut q = self
                .queued_notifications
                .lock()
                .expect("queued_notifications lock");
            q.transition(|items| items.clear())
        };
        self.publish_notification_count(generation, count);
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
            .items
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
            .items
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
            .items
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
            .items
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
        let wrap_up_notice = self
            .user_turn
            .lock()
            .expect("user_turn lock")
            .take_wrap_up_notice();
        let pending_notifications: Vec<QueuedNotification> = if pending_user.is_empty() {
            let (drained, generation, left) = {
                let mut q = self
                    .queued_notifications
                    .lock()
                    .expect("queued_notifications lock");
                let mut taken: Vec<QueuedNotification> = Vec::new();
                let (generation, left) = q.transition(|items| taken.extend(items.drain(..)));
                (taken, generation, left)
            };
            // The drain empties the queue, so the level it reports clears the
            // count the client saw before the drain. The depth comes from the
            // transition rather than a written zero, so it cannot drift from
            // what the queue actually holds.
            self.publish_notification_count(generation, left);
            drained
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
        if let Some(text) = wrap_up_notice {
            self.store
                .append(new_event(session, SessionEvent::MetaUser { text }))
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
