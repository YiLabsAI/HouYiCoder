//! Generic in-process message bus for pub/sub and point-to-point transport.
//!
//! Built on tokio primitives (broadcast for pub/sub, mpsc for inbox,
//! oneshot for RPC). The trait is generic over the message type so
//! domain layers define their own message enums without the transport
//! knowing about them.
//!
//! InProcBus is the same-process implementation; future UDS (same
//! machine) and NATS (cross-machine) implementations implement the
//! same trait so callers swap transport without touching code.

use std::collections::HashMap;
use std::sync::RwLock;

use tokio::sync::{broadcast, mpsc};

/// Generic message bus trait. The transport is dumb: it carries
/// T payloads keyed by string topics and agent IDs. Domain layers
/// define T (e.g. BusMessage in the core multi-agent module).
///
/// Three communication patterns:
/// - publish/subscribe: broadcast (multiple subscribers, fire-and-forget)
/// - inbox: mpsc (point-to-point, single consumer, parent → child)
/// - request: oneshot RPC (caller blocks for a response) — handled by
///   the caller sending an Inbox with a reply-to channel; the bus
///   itself does not own request/response pairing (kept simple)
pub trait MessageBus<T: Clone + Send + Sync + 'static>: Send + Sync {
    /// Publish to a topic. All active subscribers receive. If no
    /// subscriber exists the message is dropped silently (pub/sub
    /// fire-and-forget, not an error).
    fn publish(&self, topic: &str, message: T);

    /// Subscribe to a topic. Returns a receiver for messages published
    /// after this call (broadcast: late subscribers miss earlier messages).
    fn subscribe(&self, topic: &str) -> broadcast::Receiver<T>;

    /// Send a point-to-point message to a child's inbox. The child must
    /// have registered its inbox receiver first. Returns Err if the child
    /// is gone (inbox closed or not registered).
    fn send_inbox(&self, agent_id: &str, message: T) -> Result<(), String>;

    /// Register a child's inbox sender. Called once when the child starts.
    fn register_inbox(&self, agent_id: &str, tx: mpsc::UnboundedSender<T>);

    /// Remove a child's inbox (on completion or kill).
    fn unregister_inbox(&self, agent_id: &str);
}

/// Same-process MessageBus backed by tokio channels.
///
/// - publish/subscribe: tokio broadcast (pub/sub, multiple subscribers)
/// - inbox: tokio unbounded mpsc (point-to-point, single consumer)
///
/// The broadcast buffer is capped at 256 messages per topic; a slow
/// subscriber that falls behind receives a RecvError Lagged and
/// continues from the current tail (no blocking, no backpressure —
/// progress is fire-and-forget).
pub struct InProcBus<T: Clone + Send + Sync + 'static> {
    topics: RwLock<HashMap<String, broadcast::Sender<T>>>,
    inboxes: RwLock<HashMap<String, mpsc::UnboundedSender<T>>>,
}

const BROADCAST_CAPACITY: usize = 256;

impl<T: Clone + Send + Sync + 'static> InProcBus<T> {
    pub fn new() -> Self {
        Self {
            topics: RwLock::new(HashMap::new()),
            inboxes: RwLock::new(HashMap::new()),
        }
    }

    fn topic_sender(&self, topic: &str) -> broadcast::Sender<T> {
        let read = self.topics.read().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = read.get(topic) {
            return tx.clone();
        }
        drop(read);
        let mut write = self.topics.write().unwrap_or_else(|e| e.into_inner());
        write
            .entry(topic.to_string())
            .or_insert_with(|| broadcast::channel(BROADCAST_CAPACITY).0)
            .clone()
    }
}

impl<T: Clone + Send + Sync + 'static> Default for InProcBus<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + Send + Sync + 'static> MessageBus<T> for InProcBus<T> {
    fn publish(&self, topic: &str, message: T) {
        let tx = self.topic_sender(topic);
        // Err means no active receivers; fire-and-forget drops it.
        drop(tx.send(message));
    }

    fn subscribe(&self, topic: &str) -> broadcast::Receiver<T> {
        let tx = self.topic_sender(topic);
        tx.subscribe()
    }

    fn send_inbox(&self, agent_id: &str, message: T) -> Result<(), String> {
        let read = self.inboxes.read().unwrap_or_else(|e| e.into_inner());
        match read.get(agent_id) {
            Some(tx) => tx
                .send(message)
                .map_err(|_| format!("inbox closed for agent {agent_id}")),
            None => Err(format!("no inbox registered for agent {agent_id}")),
        }
    }

    fn register_inbox(&self, agent_id: &str, tx: mpsc::UnboundedSender<T>) {
        self.inboxes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id.to_string(), tx);
    }

    fn unregister_inbox(&self, agent_id: &str) {
        self.inboxes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_publish_subscribe() {
        let bus = InProcBus::<u32>::new();
        let mut rx = bus.subscribe("topic");
        bus.publish("topic", 42);
        assert_eq!(rx.try_recv().unwrap(), 42);
    }

    #[test]
    fn test_publish_orphan_dropped() {
        let bus = InProcBus::<u32>::new();
        bus.publish("nobody", 99);
    }

    #[test]
    fn test_late_subscriber_lags() {
        let bus = InProcBus::<u32>::new();
        bus.publish("topic", 1);
        let mut rx = bus.subscribe("topic");
        assert!(rx.try_recv().is_err());
    }

    /// Broadcast fan-out: multiple subscribers to the same topic each
    /// receive the same message. This is the core pub/sub semantic.
    #[test]
    fn test_broadcast_fanout() {
        let bus = InProcBus::<u32>::new();
        let mut rx1 = bus.subscribe("fanout");
        let mut rx2 = bus.subscribe("fanout");
        bus.publish("fanout", 7);
        assert_eq!(rx1.try_recv().unwrap(), 7);
        assert_eq!(rx2.try_recv().unwrap(), 7);
    }

    /// Registering an inbox twice for the same agent replaces the sender,
    /// orphaning the first receiver (it sees a closed channel on next
    /// recv). Pins the overwrite behavior so a future change is a
    /// deliberate decision, not an accident.
    #[test]
    fn test_inbox_reregister_replaces() {
        let bus = InProcBus::<u32>::new();
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        bus.register_inbox("dup", tx1);
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        // Second register replaces the first sender.
        bus.register_inbox("dup", tx2);
        bus.send_inbox("dup", 1).unwrap();
        // The new receiver gets the message.
        assert_eq!(rx2.try_recv().unwrap(), 1);
        // The orphaned receiver gets nothing (channel closed, not the message).
        assert!(rx1.try_recv().is_err());
    }

    #[test]
    fn test_inbox_delivers() {
        let bus = InProcBus::<u32>::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        bus.register_inbox("alice", tx);
        bus.send_inbox("alice", 7).unwrap();
        assert_eq!(rx.try_recv().unwrap(), 7);
    }

    #[test]
    fn test_inbox_unknown_agent() {
        let bus = InProcBus::<u32>::new();
        assert!(bus.send_inbox("nobody", 1).is_err());
    }

    #[test]
    fn test_unregister_inbox() {
        let bus = InProcBus::<u32>::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        bus.register_inbox("bob", tx);
        bus.unregister_inbox("bob");
        assert!(bus.send_inbox("bob", 1).is_err());
    }
}
