//! A stdio carrier for the ACP server — the real socket surface a stock ACP
//! client drives. Reads NDJSON lines from stdin (one frame per line),
//! forwards them to AcpServer over an mpsc pair, and writes the server's
//! outbound NDJSON lines to stdout. Built as the verify handle (drive it
//! with raw JSON-RPC frames + capture responses) and as the mode-B ACP
//! composition root (a pipe-carrier entry a real ACP client connects to).
//!
//! The runner is a stub provider that returns one canned reply, so a
//! session/prompt completes in a single turn with stopReason end_turn.
//! Real providers plug in behind the same AcpServer.

use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_provider::FakeProvider;
use houyicoder_service::acp_adapter::AcpAdapter;
use houyicoder_service::acp_serve::AcpIo;
use houyicoder_service::acp_server::AcpServer;
use houyicoder_service::lifecycle::SessionLeaseStore;
use houyicoder_session::SessionStore;
use std::io::{BufRead, Write};
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    // The server is single-session-bound: the runner drives this one session.
    // Print it to stderr so a client prompts against the right id (stdout
    // stays clean for the JSON-RPC frame stream). A production composition
    // root binds the runner to the id session/new mints; this stub example
    // pre-mints because the adapter's session/new mints a store-only record
    // the runner is not bound to (a gap the lifecycle runtime closes).
    eprintln!("session_id={session}");
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("hello from stub"));
    let tools = ToolRegistry::new();
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));

    // mpsc pair: one direction each way, matching the in-memory carrier.
    // Large capacity so a short turn's frames buffer without a concurrent
    // drain (the drain runs after serve returns); a streaming production
    // carrier drains concurrently to avoid blocking on a full buffer.
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    let mut io = AcpIo::new(server_tx, server_rx);

    // Bridge stdin -> client_tx (inbound frames to the server). A dedicated
    // thread does blocking read_line (tokio stdin has an event-loop blocking
    // pitfall); EOF/err drops the sender so the server sees a clean close.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut tx = client_tx;
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.try_send(l + "\n").is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Drain outbound frames to stdout CONCURRENTLY with serve, so an
    // interactive client (send a request, read its reply, then send the
    // next — without closing stdin) is not deadlocked waiting for a reply
    // that only flushes after serve returns. A spawned task writes per
    // frame via a fresh stdout() handle (StdoutLock is !Send, so the lock
    // is not held across the await).
    let mut out_rx = client_rx;
    let drain = tokio::spawn(async move {
        while let Some(frame) = out_rx.next().await {
            let mut out = std::io::stdout();
            drop(out.write_all(frame.as_bytes()));
            drop(out.write_all(b"\n"));
            drop(out.flush());
        }
    });
    let server = AcpServer::new(adapter, runner, session);
    drop(server.serve(&mut io).await);
    // serve borrows io by &mut; drop io so client_rx's sender closes,
    // the drain task ends, and the process exits.
    drop(io);
    drop(drain.await);
}
