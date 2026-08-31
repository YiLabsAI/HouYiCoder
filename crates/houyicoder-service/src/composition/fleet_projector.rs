//! The fleet projector: a service-side bridge that subscribes to the
//! multi-agent bus and translates child progress and completion into
//! AgentStatus wire frames the TUI renders as the agent status footer.
//! Lives in the service so the TUI (presentation) stays bus-free.
//!
//! Single task over the global spawned, progress, and completion topics.
//! Subscribing once at startup guarantees the one-shot terminal event is
//! never lost to a per-child subscribe landing after the child completed.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::SinkExt;
use futures::channel::mpsc;

use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildStatus, global_completed_topic, global_progress_topic, spawned_topic,
};
use houyicoder_protocol::envelope::{EventEnvelope, EventSeq, ServerFrame};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::event_kind::FrontendEventKind;

/// Spawn the fleet projector on the runtime. It subscribes to the global
/// spawned, progress, and completion topics and emits an AgentStatus wire
/// frame per state change. No-op when the bus is absent (non-multi-agent
/// runs).
pub fn spawn(
    bus: Option<Arc<AgentBus>>,
    out_tx: mpsc::Sender<String>,
    next_seq: Arc<AtomicU64>,
    runtime: tokio::runtime::Handle,
) {
    let Some(bus) = bus else {
        return;
    };
    let mut spawned_rx = bus.subscribe(spawned_topic());
    let mut progress_rx = bus.subscribe(global_progress_topic());
    let mut completed_rx = bus.subscribe(global_completed_topic());
    runtime.spawn(async move {
        let mut out_tx = out_tx;
        // agent_id -> (subagent_type, last snapshot). Spawned inserts a running
        // snapshot; Progress updates it; Completed sets the terminal status
        // and removes the entry. Bounded to in-flight children.
        let mut children: HashMap<String, (String, Snapshot)> = HashMap::new();
        loop {
            // Biased: Spawned before Progress before Completed when several
            // are ready, so a child's first frame is its running snapshot.
            tokio::select! {
                biased;
                msg = spawned_rx.recv() => match msg {
                    Ok(BusMessage::Spawned { agent_id, subagent_type, run_in_background: _ }) => {
                        let snap = Snapshot { turn: 0, tokens: 0, tool_uses: 0, last_activity: None, completed: None };
                        emit(&mut out_tx, &next_seq, &agent_id, &subagent_type, &snap).await;
                        children.insert(agent_id, (subagent_type, snap));
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                msg = progress_rx.recv() => match msg {
                    Ok(BusMessage::Progress { agent_id, turn, tokens, tool_uses, last_activity }) => {
                        if let Some((subagent_type, snap)) = children.get_mut(&agent_id) {
                            *snap = Snapshot { turn, tokens, tool_uses, last_activity, completed: None };
                            emit(&mut out_tx, &next_seq, &agent_id, subagent_type, snap).await;
                        }
                        // A progress frame for an unknown child is a Spawned
                        // lost to broadcast lag; the next frame still renders.
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                msg = completed_rx.recv() => match msg {
                    Ok(BusMessage::Completed { agent_id, status, summary, .. }) => {
                        if let Some((subagent_type, snap)) = children.get_mut(&agent_id) {
                            snap.completed = Some(status_str(&status));
                            snap.last_activity = Some(summary);
                            emit(&mut out_tx, &next_seq, &agent_id, subagent_type, snap).await;
                            children.remove(&agent_id);
                        }
                        // A completion for an unknown child is a Spawned lost
                        // to lag; the parent log's SubagentReturn boundary
                        // still records the result.
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
}

struct Snapshot {
    turn: u32,
    tokens: u64,
    tool_uses: u32,
    last_activity: Option<String>,
    completed: Option<String>,
}

fn status_str(status: &ChildStatus) -> String {
    match status {
        ChildStatus::Completed => "completed",
        ChildStatus::Killed => "killed",
        ChildStatus::Failed => "failed",
        ChildStatus::TurnLimit => "turn_limit",
        ChildStatus::BudgetExhausted => "budget",
    }
    .to_string()
}

async fn emit(
    out_tx: &mut mpsc::Sender<String>,
    next_seq: &Arc<AtomicU64>,
    agent_id: &str,
    subagent_type: &str,
    snap: &Snapshot,
) {
    let seq = next_seq.fetch_add(1, Ordering::Relaxed);
    let frame = ServerFrame::Event(EventEnvelope::new(
        EventSeq(seq),
        FrontendEventKind::AgentStatus {
            agent_id: agent_id.to_string(),
            subagent_type: subagent_type.to_string(),
            turn: snap.turn,
            tokens: snap.tokens,
            tool_uses: snap.tool_uses,
            last_activity: snap.last_activity.clone(),
            completed: snap.completed.clone(),
        },
    ));
    let Ok(line) = encode(&frame) else { return };
    // AgentStatus is load-bearing pill state, not an ephemeral preview a
    // later frame replaces, so a full channel backpressures instead of
    // dropping the marker. The projector owns its task, so blocking only
    // delays the pill update; broadcast buffers messages while send awaits.
    let _ = out_tx.send(line).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use futures::StreamExt;
    use houyicoder_protocol::envelope::ServerFrame;
    use houyicoder_protocol::frontend::event_kind::FrontendEventKind;

    /// A Spawned + Progress sequence emits a running frame then a progress
    /// frame, both demuxed from the global topics by agent_id.
    #[tokio::test]
    async fn test_projector_translates_progress() {
        let bus = Arc::new(AgentBus::new());
        let (tx, mut rx) = mpsc::channel(16);
        let next_seq = Arc::new(AtomicU64::new(0));
        spawn(
            Some(bus.clone()),
            tx,
            next_seq,
            tokio::runtime::Handle::current(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                agent_id: "c1".into(),
                subagent_type: "explore".into(),
                run_in_background: false,
            },
        );
        tokio::task::yield_now().await;
        // The first frame is the running snapshot from Spawned.
        drop(rx.next().await.expect("running frame"));
        bus.publish(
            global_progress_topic(),
            BusMessage::Progress {
                agent_id: "c1".into(),
                turn: 1,
                tokens: 100,
                tool_uses: 2,
                last_activity: Some("grep".into()),
            },
        );
        let line = rx.next().await.expect("progress frame received");
        let frame: ServerFrame = serde_json::from_str(&line).unwrap();
        match frame {
            ServerFrame::Event(ev) => match ev.payload {
                FrontendEventKind::AgentStatus { agent_id, turn, .. } => {
                    assert_eq!(agent_id, "c1");
                    assert_eq!(turn, 1);
                }
                other => panic!("expected AgentStatus, got {other:?}"),
            },
            other => panic!("expected Event, got {other:?}"),
        }
    }

    /// Race boundary: a child that completes immediately after spawning must
    /// still emit a terminal frame. This was the per-child subscribe failure
    /// (a completion before the per-child subscribe landed left the pill stuck
    /// on running). The global subscribe-before-publish guarantee makes the
    /// loss structurally impossible.
    #[tokio::test]
    async fn test_projector_immediate_completion() {
        let bus = Arc::new(AgentBus::new());
        let (tx, mut rx) = mpsc::channel(16);
        let next_seq = Arc::new(AtomicU64::new(0));
        spawn(
            Some(bus.clone()),
            tx,
            next_seq,
            tokio::runtime::Handle::current(),
        );
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                agent_id: "c2".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        bus.publish(
            global_completed_topic(),
            BusMessage::Completed {
                agent_id: "c2".into(),
                status: ChildStatus::Completed,
                summary: "done".into(),
                subagent_type: "explore".into(),
                run_in_background: true,
            },
        );
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        // Drain frames: the running snapshot then the terminal frame.
        let mut saw_terminal = false;
        while let Ok(line) = rx.try_recv() {
            let frame: ServerFrame = serde_json::from_str(&line).unwrap();
            if let ServerFrame::Event(ev) = frame
                && let FrontendEventKind::AgentStatus {
                    completed: Some(c), ..
                } = ev.payload
            {
                assert_eq!(c, "completed");
                saw_terminal = true;
            }
        }
        assert!(
            saw_terminal,
            "immediate completion must still emit a terminal frame"
        );
    }

    /// A full outbound channel must not drop an AgentStatus frame. The pill
    /// state is load-bearing, not an ephemeral preview, so the projector
    /// backpressures (send.await) instead of try_send-dropping. This is the
    /// regression guard for the real-provider streaming case: delta try_sends
    /// saturate the shared s2c channel, and a try_send projector would lose
    /// every spawn/progress frame.
    ///
    /// Distinguishing mutation: poll emit once on a FULL channel. Under
    /// send().await the future parks Pending (now_or_never None); under
    /// try_send it drops the frame and returns Ready (now_or_never Some).
    /// A try_send revert makes the assertion fail. No spawn race: the poll
    /// is synchronous and single-step, so the frame cannot slip past the
    /// full channel into an emptied slot.
    #[tokio::test]
    async fn test_projector_backpressures_full_channel() {
        let (mut tx, mut rx) = mpsc::channel(1);
        let next_seq = Arc::new(AtomicU64::new(0));
        // Fill the single slot so the next send faces a full channel.
        tx.try_send("placeholder".into()).unwrap();
        let snap = Snapshot {
            turn: 2,
            tokens: 50,
            tool_uses: 1,
            last_activity: None,
            completed: None,
        };
        let mut emit_fut = Box::pin(emit(&mut tx, &next_seq, "c1", "explore", &snap));
        // One poll on the full channel: send().await stays pending; try_send
        // would drop the frame and return Ready.
        assert!(
            emit_fut.as_mut().now_or_never().is_none(),
            "emit must block on a full channel, not drop the frame and return"
        );
        // Free the slot; the parked send wakes and delivers the frame.
        let _placeholder = rx.next().await.expect("placeholder drained");
        emit_fut.as_mut().await;
        let line = rx
            .next()
            .await
            .expect("frame delivered after capacity freed");
        let frame: ServerFrame = serde_json::from_str(&line).unwrap();
        if let ServerFrame::Event(ev) = frame
            && let FrontendEventKind::AgentStatus { turn, .. } = ev.payload
        {
            assert_eq!(turn, 2, "the saturated-channel frame kept its payload");
        } else {
            panic!("expected AgentStatus after saturation");
        }
    }
}
