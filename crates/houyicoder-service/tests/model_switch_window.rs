//! A model switch that shrinks the resolved context window: the pre-flight
//! gate resolves the window from the ACTIVE model id on every call, so a
//! session that fit under the old model's wide window must compact (not
//! overflow) on the first turn after the switch. Integration test (real
//! Runner + Server + serve loop + a canned provider), so it lives in tests/.
//!
//! The provider negotiates no window (context_window 0 - the common
//! OpenAI-compatible gateway case whose models-list omits the context-length
//! field), so the window resolves client-side: a [1m] suffix opts into the
//! 1M window, the bare id falls to the conservative 200k default. The
//! history is sized to sit between the two pre-flight thresholds (~179k and
//! ~979k), so the same served view runs clean under the wide window and
//! trips the ceiling under the narrow one - the differential proves the
//! switch flipped the gate, not a change in the history.

mod common;

use std::sync::Arc;

use houyicoder_api::provider::{ModelProvider, stream_from_response};
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, ModelCapabilities, OutputItem, ProviderError, Usage,
};
use houyicoder_service::server::Server;
use houyicoder_session::SessionStore;

use common::{pair, recv_frame_within, recv_hello, send_frame};

/// A canned-response provider that negotiates no context window, so the
/// runner resolves the window from the active model id through the
/// client-side table - the same resolution path production takes against a
/// gateway that omits the context-length field.
struct WindowlessProvider;

impl WindowlessProvider {
    fn response() -> CompletionResponse {
        CompletionResponse {
            output: vec![OutputItem::Text { text: "ok".into() }],
            usage: Usage::default(),
            model: "test".into(),
        }
    }
}

impl ModelProvider for WindowlessProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async move { Ok(Self::response()) })
    }

    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>> {
        stream_from_response(Self::response())
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_window: 0,
            ..ModelCapabilities::default()
        }
    }
}

/// A unique temp settings path so the ModelSet's persist writes a temp file,
/// not the developer's real settings. The file need not pre-exist; the
/// persist creates it.
fn temp_settings(slug: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "model-window-{slug}-{n}-{}.json",
        std::process::id()
    ))
}

/// Spawn serve + handshake. Returns the client channel halves.
async fn handshake(
    server: Server,
) -> (
    futures::channel::mpsc::Sender<String>,
    futures::channel::mpsc::Receiver<String>,
) {
    let (io, mut client_tx, mut client_rx) = pair();
    tokio::spawn(async move { server.serve(io).await });
    send_frame(&mut client_tx, &Hello::local()).await;
    let _ = recv_hello(&mut client_rx).await;
    (client_tx, client_rx)
}

/// Drain streamed events until the response for the given request id lands.
async fn wait_response(
    rx: &mut futures::channel::mpsc::Receiver<String>,
    req_id: RequestId,
) -> ResponsePayload {
    loop {
        match recv_frame_within(rx, std::time::Duration::from_secs(10)).await {
            ServerFrame::Response(env) if env.req_id == req_id => return env.payload,
            _ => continue,
        }
    }
}

/// Pre-append six rounds of history where round one carries a huge user
/// input. The huge round is old enough that a compact folds it (the fold
/// keeps only the last four assistant rounds verbatim), so the compact
/// makes real progress. Everything else stays small so the post-compact
/// view lands far under the narrow window's threshold.
async fn append_history(store: &SessionStore, session: SessionId) {
    // ~2.4M chars: ~600k tokens at the harness's char-based estimate,
    // ~400k under the real tokenizer - comfortably between the narrow
    // window's threshold (~179k) and the wide window's (~979k) under both
    // counting modes.
    let huge = "alpha beta gamma delta ".repeat(100_000);
    let rounds = [
        (huge.as_str(), "acknowledged"),
        ("next step", "done"),
        ("another step", "done"),
        ("keep going", "done"),
        ("almost there", "done"),
        ("final step", "done"),
    ];
    for (user, assistant) in rounds {
        store
            .append(TurnEvent {
                id: EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::UserInput {
                    text: user.to_string(),
                },
            })
            .await
            .unwrap();
        store
            .append(TurnEvent {
                id: EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::AssistantMessage {
                    text: assistant.to_string(),
                    thinking: None,
                },
            })
            .await
            .unwrap();
    }
}

/// A model switch that shrinks the resolved window compacts the session on
/// the next turn instead of overflowing: the same served view runs clean
/// under the wide window (no checkpoint written), trips the narrow window's
/// pre-flight threshold after the switch, compacts, and the turn still
/// completes.
#[tokio::test]
async fn test_switch_shrunk_window_compacts() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    append_history(&store, session).await;

    let runner = Arc::new(Runner::with_shared_store(
        store.clone(),
        Arc::new(WindowlessProvider),
        ToolRegistry::new(),
        RunnerConfig {
            model: "glm-4.6[1m]".into(),
            instructions: "test".into(),
            // One turn per run: the run budget has no remaining turns, which
            // keeps the cost-saving compact gate out of the picture so the
            // window ceiling gate is the one under test.
            max_turns: 1,
            ..RunnerConfig::default()
        },
    ));
    let (mut client_tx, mut client_rx) = handshake(
        Server::new(runner, session, Arc::new(DefaultModeGate::new()))
            .with_settings_path(temp_settings("shrink")),
    )
    .await;
    let wire_session_id = houyicoder_protocol::frontend::SessionId(session.to_string());

    // Turn one under the [1m] wide window: the view sits under the wide
    // threshold, so the run completes with no compaction.
    let run_wide = RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::MessageSend {
            session_id: wire_session_id.clone(),
            content: vec![ContentBlock::Text {
                text: "and then?".into(),
            }],
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(run_wide)).await;
    assert!(
        matches!(
            wait_response(&mut client_rx, RequestId(1)).await,
            ResponsePayload::RunOk(_)
        ),
        "the view fits the wide window, so the first run completes"
    );
    assert!(
        store.list_checkpoints(session).await.unwrap().is_empty(),
        "the same view under the wide window must not compact"
    );

    // Switch to the same model without the [1m] opt-in: the resolved window
    // drops to the conservative default, and the threshold with it.
    let switch = RequestEnvelope::new(
        RequestId(2),
        FrontendRequest::ModelSet {
            model: Some("glm-4.6".into()),
            effort: None,
            effort_toggled: false,
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(switch)).await;
    match wait_response(&mut client_rx, RequestId(2)).await {
        ResponsePayload::ModelResult(applied) => {
            assert_eq!(applied.model, "glm-4.6", "the pick applied");
        }
        other => panic!("expected ModelResult, got {other:?}"),
    }

    // Turn two: identical history, narrow window - the pre-flight gate
    // trips, compacts (folding the huge round away), and the turn still
    // completes instead of surfacing an overflow error.
    let run_narrow = RequestEnvelope::new(
        RequestId(3),
        FrontendRequest::MessageSend {
            session_id: wire_session_id.clone(),
            content: vec![ContentBlock::Text {
                text: "where are we?".into(),
            }],
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(run_narrow)).await;
    assert!(
        matches!(
            wait_response(&mut client_rx, RequestId(3)).await,
            ResponsePayload::RunOk(_)
        ),
        "the shrunken window must compact and complete, not overflow"
    );
    assert!(
        !store.list_checkpoints(session).await.unwrap().is_empty(),
        "the switch-shrunken window must have compacted the session"
    );
}
