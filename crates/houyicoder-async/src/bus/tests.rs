//! The in-process bus: publish, drain, and the closed-channel case.

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

/// The receiver count follows subscription liveness: each subscribe
/// adds one receiver, each drop removes one, and a topic nobody ever
/// subscribed reports zero.
#[test]
fn test_receiver_count_liveness() {
    let bus = InProcBus::<u32>::new();
    assert_eq!(bus.receiver_count("topic"), 0);
    let rx1 = bus.subscribe("topic");
    let rx2 = bus.subscribe("topic");
    assert_eq!(bus.receiver_count("topic"), 2);
    drop(rx1);
    assert_eq!(bus.receiver_count("topic"), 1);
    drop(rx2);
    assert_eq!(bus.receiver_count("topic"), 0);
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
