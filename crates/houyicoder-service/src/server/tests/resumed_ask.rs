//! A child's permission ask during the resumed leg of a turn.
//!
//! A turn that stops for approval runs its first leg through one wait loop and
//! every leg after it through another. The second loop was built without the
//! child permission receiver, so a child spawned after an approval published
//! its ask to a topic nobody held: no card reached the user, and the child
//! waited until its own ceiling ended it. This file pins the receiver on the
//! resumed leg by asserting the card the user would answer.

#![cfg(test)]

use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::SessionId as EngineSessionId;
use houyicoder_core::agent::ApprovalDecision;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_request_topic,
};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::ServerFrame;
use houyicoder_protocol::envelope::ServerRequestPayload;
use houyicoder_protocol::llm::CompletionResponse;
use houyicoder_protocol::llm::OutputItem;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;

use super::super::Server;
use super::super::frame_carrier::FrameCarrier;

/// A server on a real bus with one runner, so the resumed leg's wait loop has
/// a receiver to subscribe and a topic to publish on.
///
/// The provider holds its answer for a while: a resumed leg that finishes at
/// once tears its receiver down before a test can observe the wait, which is
/// the window this file is about.
fn server_with_bus() -> (Server, Arc<AgentBus>, EngineSessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider = FakeProvider::new_with_delay(
        vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Default::default(),
            model: "test".into(),
        }],
        1500,
    );
    let provider: Arc<dyn ModelProvider> = Arc::new(provider);
    let runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let session = EngineSessionId::new();
    let bus = Arc::new(AgentBus::new());
    let server = Server::new(Arc::new(runner), session, Arc::new(DefaultModeGate::new()))
        .with_bus(Some(Arc::clone(&bus)));
    (server, bus, session)
}

/// An ask published while the resumed leg waits reaches the wire as a
/// permission card.
///
/// This is the defect the receiver closes. Two weaker observations do not
/// catch it: counting subscribers passes for a subscription made and then
/// dropped on the way into the wait loop, and publishing before the loop runs
/// lands on a buffered topic with no subscriber at all. So the ask is
/// published only once the topic has a subscriber, and the assertion is on
/// the card that reaches the wire.
#[tokio::test]
async fn test_resumed_leg_raises_card() {
    let (mut server, bus, _session) = server_with_bus();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(16);
    let (_client_tx, server_rx) = mpsc::channel::<String>(16);
    let mut io = FrameCarrier::new(server_tx, server_rx);

    let topic = permission_request_topic();
    let decisions: Vec<ApprovalDecision> = Vec::new();
    let drive = server.drive_resume_step(&mut io, decisions.as_slice());
    tokio::pin!(drive);

    // The leg has to be polled for its wait loop to run at all, so the leg and
    // the publish run inside one select rather than one after the other.
    let card = timeout(Duration::from_secs(3), async {
        // Wait for the wait loop to take its receiver first. Publishing before
        // that would land on a buffered topic with no subscriber and pass
        // without the loop ever hearing it.
        loop {
            tokio::select! {
                _ = &mut drive => panic!("the leg ended before it subscribed"),
                _ = sleep(Duration::from_millis(5)) => {
                    if bus.receiver_count(topic) > 0 {
                        break;
                    }
                }
            }
        }
        bus.publish(
            topic,
            BusMessage::PermissionRequest {
                child_id: "child-1".into(),
                subagent_type: "explore".into(),
                call_id: "call-1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "git stash"}),
                reason: None,
            },
        );

        loop {
            tokio::select! {
                _ = &mut drive => panic!("the leg ended before a card arrived"),
                line = client_rx.next() => match line {
                    Some(line) => {
                        let frame: ServerFrame =
                            serde_json::from_str(&line).expect("frame decodes");
                        if let ServerFrame::Request(request) = frame {
                            return request;
                        }
                    }
                    None => panic!("the wire closed before a card arrived"),
                },
            }
        }
    })
    .await
    .expect("the ask reaches the wire instead of waiting out the child's ceiling");

    match card.payload {
        ServerRequestPayload::Permission(ask) => {
            assert_eq!(
                ask.call_id, "call-1",
                "the card names the child's call, so an answer can be routed back"
            );
        }
        other => panic!("expected a permission card, got {other:?}"),
    }
}
