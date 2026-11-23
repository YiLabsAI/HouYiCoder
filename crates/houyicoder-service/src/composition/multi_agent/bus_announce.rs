//! The child announcements the fleet reads: a child is announced before its
//! first status event can be published, and its inbox closes when the run
//! ends so a parent's send refuses rather than queueing into a dead receiver.

use std::sync::Arc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildDescriptor, spawned_topic,
};

/// Announce a child before its first status event can be published.
pub(super) fn announce_spawn(bus: Option<&Arc<AgentBus>>, child: ChildDescriptor) {
    if let Some(bus) = bus {
        bus.publish(spawned_topic(), BusMessage::Spawned { child });
    }
}

/// Close a child's inbox after its run so a parent's send_inbox errors
/// rather than queueing into a dead receiver. Mirrors announce_spawn.
pub(super) fn close_child_inbox(bus: Option<&Arc<AgentBus>>, child_id: &str) {
    if let Some(bus) = bus {
        bus.unregister_inbox(child_id);
    }
}
