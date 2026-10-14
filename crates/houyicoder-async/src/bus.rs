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

    /// How many live receivers hold a subscription to a topic; a topic
    /// nobody ever subscribed reports zero. The count is the observable
    /// footprint of a consumer: a consumer that ends drops its receiver,
    /// so a count falling to zero shows the consumer stopped without
    /// naming the task or its join order. Inherent rather than a trait
    /// method because it reads the in-process channel table; transports
    /// that broker remotely would answer it differently.
    pub fn receiver_count(&self, topic: &str) -> usize {
        let read = self.topics.read().unwrap_or_else(|e| e.into_inner());
        read.get(topic).map_or(0, |tx| tx.receiver_count())
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
mod tests;
