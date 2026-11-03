//! The recap contract across the connection: a session reopened after a while
//! is summarized once, the summary reaches the attached client as its own
//! notification, and a reattaching client is handed the stored row without a
//! second request. A terminal that stops looking at the session lets the host
//! summarize it after the away window. A recap that fails leaves only its
//! attempt record.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures::channel::mpsc;
use futures::stream;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_async::PFut;
use houyicoder_async::PStream;
use houyicoder_context::EventId;
use houyicoder_context::RecapSource;
use houyicoder_context::SessionEvent;
use houyicoder_context::SessionId;
use houyicoder_context::SessionLogEntry;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::envelope::ServerFrame;
use houyicoder_protocol::error::ProtocolError;
use houyicoder_protocol::frontend::FrontendEvent;
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::CompletionRequest;
use houyicoder_protocol::llm::CompletionResponse;
use houyicoder_protocol::llm::LlmEvent;
use houyicoder_protocol::llm::ModelCapabilities;
use houyicoder_protocol::llm::OutputItem;
use houyicoder_protocol::llm::ProviderError;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::UsageProvenance;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::Server;
use houyicoder_service::server::recap::AWAY_AFTER;
use houyicoder_service::server::recap::Entry;
use houyicoder_service::server::recap::OUTPUT_TOKENS;
use houyicoder_service::server::recap::RecapSession;
use houyicoder_service::server::recap::RecapSettings;
use houyicoder_session::SessionStore;
use serde_json::json;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::task::yield_now;
use tokio::time::sleep;
use tokio::time::timeout;

use crate::common::pair;
use crate::common::recv_hello;
use crate::common::recv_line;
use crate::common::send_frame;

/// What the recap endpoint answers when a recap is written.
const REPLY: &str = "You are building the recap path. Next: run the contract test.";

/// A provider for recap requests: it answers with one fixed sentence or fails
/// as told, counts its calls, and keeps the last request so a test can assert
/// what the host actually sent.
struct RecapProvider {
    reply: Result<String, ProviderError>,
    calls: AtomicU32,
    last: Mutex<Option<CompletionRequest>>,
}

impl RecapProvider {
    fn new(reply: Result<&str, ProviderError>) -> Self {
        Self {
            reply: reply.map(str::to_string),
            calls: AtomicU32::new(0),
            last: Mutex::new(None),
        }
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    /// Take the request the host last sent, so a test reads it once.
    fn take_request(&self) -> CompletionRequest {
        self.last
            .lock()
            .expect("provider lock")
            .take()
            .expect("the host sent a request")
    }
}

impl ModelProvider for RecapProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().expect("provider lock") = Some(req);
        let reply = self.reply.clone();
        Box::pin(async move {
            reply.map(|text| CompletionResponse {
                output: vec![OutputItem::Text { text }],
                usage: Usage {
                    input_tokens: 120,
                    output_tokens: 7,
                    provenance: UsageProvenance {
                        input_reported: true,
                        ..UsageProvenance::default()
                    },
                    ..Usage::default()
                },
                model: "small".to_string(),
            })
        })
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        Box::pin(stream::empty())
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// One session built the way the composition root builds a recap: the state,
/// the provider it spends on, and the store the runner and the recap share.
struct Hosted {
    store: Arc<SessionStore>,
    session: SessionId,
    runner: Arc<Runner>,
    recap: Arc<RecapSession>,
    provider: Arc<RecapProvider>,
    notify: Arc<Notify>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock reads after the epoch")
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Seed work the user did a while ago. The resume entry only summarizes a
/// session that has been idle long enough to have been walked away from.
async fn seed_old_work(store: &SessionStore, session: SessionId) {
    let ts = now_ms().saturating_sub(10 * 60 * 1000);
    for event in [
        SessionEvent::UserInput {
            text: "build the recap path".into(),
        },
        SessionEvent::AssistantMessage {
            text: "the host state is in place".into(),
            thinking: None,
        },
    ] {
        let entry = SessionLogEntry {
            id: EventId::new(),
            session,
            ts,
            prev_hash: None,
            event,
        };
        store.append(entry).await.expect("seed the log");
    }
}

async fn hosted(reply: Result<&str, ProviderError>) -> Hosted {
    let notify = Arc::new(Notify::new());
    let store = Arc::new(
        SessionStore::new(Box::new(InMemoryBackend::new())).with_append_notify(Arc::clone(&notify)),
    );
    let session = SessionId::new();
    seed_old_work(&store, session).await;
    let provider = Arc::new(RecapProvider::new(reply));
    let log: Arc<dyn SessionLog> = store.clone();
    let recap_provider: Arc<dyn ModelProvider> = provider.clone();
    let recap = Arc::new(RecapSession::new(
        session,
        RecapSettings {
            enabled: true,
            model: Some("small".into()),
            ..Default::default()
        },
        recap_provider,
        Arc::clone(&log),
    ));
    let runner_provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("unused"));
    let runner = Arc::new(Runner::new(
        log,
        runner_provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    Hosted {
        store,
        session,
        runner,
        recap,
        provider,
        notify,
    }
}

/// The server one connection gets, built the way a host rebuilds it on
/// attach.
fn server_for(hosted: &Hosted) -> Server {
    Server::new(
        Arc::clone(&hosted.runner),
        hosted.session,
        Arc::new(DefaultModeGate::new()),
    )
    .with_append_notify(Arc::clone(&hosted.notify))
    .with_recap(Arc::clone(&hosted.recap))
}

/// Read frames until the recap notification arrives, then return its text.
/// The other frames on the stream are skipped: where the recap sits among
/// them is not part of its contract. The wait is bounded by the caller's
/// budget, which a test on a paused clock sizes against the window it crosses.
async fn read_recap_text(rx: &mut mpsc::Receiver<String>, within: Duration) -> String {
    let mut seen: Vec<String> = Vec::new();
    let recap = timeout(within, async {
        loop {
            let line = recv_line(rx).await;
            let frame: ServerFrame = serde_json::from_str(&line).expect("server frame decodes");
            if let ServerFrame::Event(envelope) = frame
                && let FrontendEvent::Acpx { notification } = envelope.payload
                && notification.method == AcpxMethod::ContextRecap
            {
                return notification.params["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
            }
            seen.push(line);
        }
    })
    .await;
    match recap {
        Ok(text) => text,
        Err(_) => panic!("no recap notification within {within:?}; the frames read were {seen:#?}"),
    }
}

/// Serve one connection until the recap notification lands, then close the
/// connection and return the text the client read.
async fn serve_and_read_recap(hosted: &Hosted) -> String {
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = server_for(hosted);
    let handle = tokio::spawn(async move { server.serve(server_io).await });
    send_frame(&mut client_tx, &Hello::local()).await;
    recv_hello(&mut client_rx).await;
    let text = read_recap_text(&mut client_rx, Duration::from_secs(5)).await;
    drop(client_tx);
    join_server(handle).await;
    text
}

/// Wait for the serve loop to finish on its own and report that it did: a
/// loop that returned an error is a carrier failure the test must not hide.
async fn join_server(handle: JoinHandle<Result<(), ProtocolError>>) {
    let result = handle.await.expect("the serve loop joins");
    result.expect("the serve loop ends cleanly");
}

/// Wait for the recap job for entry to finish. An assertion about a row that
/// must be absent is only final once the job is over, since the job is what
/// would write it.
async fn wait_until_settled(recap: &RecapSession, entry: Entry) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while recap.is_running(entry) {
        assert!(Instant::now() < deadline, "the recap job never finished");
        yield_now().await;
    }
}

/// Wait for the log to hold an event matching the predicate, then return the
/// replay. The store's own append signal is the wake, so the wait ends as
/// soon as the write lands.
async fn wait_for_event(
    notify: &Notify,
    store: &SessionStore,
    session: SessionId,
    want: impl Fn(&SessionEvent) -> bool,
) -> Vec<SessionLogEntry> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = store.replay(session).await.expect("replay the log");
        if events.iter().any(|entry| want(&entry.event)) {
            return events;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            () = notify.notified() => {}
            () = sleep(remaining) => panic!("the expected event never landed"),
        }
    }
}

/// Reopening an idle session sends one bounded request, and its answer
/// reaches the client with the request's cost recorded beside it.
#[tokio::test]
async fn test_resume_recap_reaches_client() {
    let hosted = hosted(Ok(REPLY)).await;
    hosted.recap.resume_entry();

    let text = serve_and_read_recap(&hosted).await;
    assert_eq!(text, REPLY, "the client reads the model's sentence");

    let request = hosted.provider.take_request();
    assert!(
        request.tools.is_empty(),
        "a recap describes work instead of starting some"
    );
    assert_eq!(request.settings.max_output_tokens, Some(OUTPUT_TOKENS));
    assert_eq!(request.settings.enable_thinking, Some(false));
    assert_eq!(hosted.provider.calls(), 1, "one request per recap");

    let events = wait_for_event(&hosted.notify, &hosted.store, hosted.session, |event| {
        matches!(event, SessionEvent::AwayRecap { .. })
    })
    .await;
    let recap = events
        .iter()
        .map(|entry| &entry.event)
        .find(|event| matches!(event, SessionEvent::AwayRecap { .. }))
        .expect("the recap is written");
    let SessionEvent::AwayRecap {
        text: stored,
        source,
        model,
        usage,
        ..
    } = recap
    else {
        unreachable!("the finder matched the recap")
    };
    assert_eq!(stored, REPLY);
    assert_eq!(*source, RecapSource::Resume);
    assert_eq!(model, "small");
    let usage = usage.as_ref().expect("the request's cost is recorded");
    assert_eq!(usage.input_tokens, Some(120));
    assert_eq!(usage.output_tokens, Some(7));
    assert_eq!(
        events
            .iter()
            .filter(|entry| matches!(entry.event, SessionEvent::RecapAttempt { .. }))
            .count(),
        1,
        "the attempt is recorded once, beside the answer"
    );
}

/// A client that reattaches is handed the stored recap, and the host spends
/// nothing: the summary belongs to the point in the session it described.
#[tokio::test]
async fn test_reattach_replays_recap_once() {
    let hosted = hosted(Ok(REPLY)).await;
    hosted.recap.resume_entry();

    assert_eq!(serve_and_read_recap(&hosted).await, REPLY);
    assert_eq!(serve_and_read_recap(&hosted).await, REPLY);
    assert_eq!(
        hosted.provider.calls(),
        1,
        "replaying a stored recap spends nothing"
    );
}

/// A request that fails leaves the attempt record and no row: the attempt is
/// what stops a second silent spend, and the absent recap keeps the failure
/// off the transcript.
#[tokio::test]
async fn test_failed_recap_leaves_attempt() {
    let hosted = hosted(Err(ProviderError::Network)).await;
    hosted.recap.resume_entry();

    let (server_io, mut client_tx, _client_rx) = pair();
    let server = server_for(&hosted);
    let handle = tokio::spawn(async move { server.serve(server_io).await });
    send_frame(&mut client_tx, &Hello::local()).await;

    wait_for_event(&hosted.notify, &hosted.store, hosted.session, |event| {
        matches!(event, SessionEvent::RecapAttempt { .. })
    })
    .await;
    // The attempt is written before the request goes out, so the row could
    // still be mid-flight here. Wait the job out before asserting its row is
    // absent, or the assertion reads a state the job has not reached.
    wait_until_settled(&hosted.recap, Entry::Resume).await;
    assert_eq!(hosted.provider.calls(), 1);
    let events = hosted.store.replay(hosted.session).await.expect("replay");
    assert!(
        events
            .iter()
            .all(|entry| !matches!(entry.event, SessionEvent::AwayRecap { .. })),
        "a failed recap draws no row"
    );

    let attempt = events
        .iter()
        .map(|entry| &entry.event)
        .find(|event| matches!(event, SessionEvent::RecapAttempt { .. }))
        .expect("the attempt is recorded before the request");
    let SessionEvent::RecapAttempt { source, model, .. } = attempt else {
        unreachable!("the finder matched the attempt")
    };
    assert_eq!(*source, RecapSource::Resume);
    assert_eq!(model, "small");

    drop(client_tx);
    join_server(handle).await;
}

/// A terminal that stops looking at the session lets the host summarize it
/// once the away window passes, and the answer reaches that terminal as its
/// own notification. The state machine's clock is the runtime's, so the test
/// crosses the window without waiting it out.
#[tokio::test(start_paused = true)]
async fn test_away_recap_reaches_client() {
    let hosted = hosted(Ok(REPLY)).await;
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = server_for(&hosted);
    let handle = tokio::spawn(async move { server.serve(server_io).await });
    send_frame(&mut client_tx, &Hello::local()).await;
    recv_hello(&mut client_rx).await;
    send_frame(
        &mut client_tx,
        &AcpNotification::new("session/focus", json!({ "focused": false })),
    )
    .await;

    let text = read_recap_text(&mut client_rx, AWAY_AFTER * 2).await;
    assert_eq!(text, REPLY, "the client reads the model's sentence");
    assert_eq!(hosted.provider.calls(), 1, "one request per recap");

    // The notification is shipped after the row is written, so reading the
    // log now sees what the client was just told.
    let events = hosted.store.replay(hosted.session).await.expect("replay");
    let source = events.iter().find_map(|entry| match &entry.event {
        SessionEvent::AwayRecap { source, .. } => Some(*source),
        _ => None,
    });
    assert_eq!(
        source,
        Some(RecapSource::Away),
        "an unfocused terminal is the away entry, not the resume one"
    );

    drop(client_tx);
    join_server(handle).await;
}
