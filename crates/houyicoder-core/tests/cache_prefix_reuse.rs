//! Prefix reuse: the request bytes the endpoint receives, turn over turn.
//!
//! The live measurement in the composition root reads the endpoint's side of
//! prompt caching, how much of each request came back as a cache read. This
//! suite reads the client's side, whether the message list sent this turn still
//! holds the one sent the turn before. A value that drifts inside that list
//! breaks reuse while every count the client can see stays plausible, so the
//! drift is caught here against a stand-in endpoint instead of being inferred
//! from a ratio.
//!
//! The stand-in keeps one cached entry per cache key and answers each request
//! with the leading bytes it shares with the body that filled that entry. Those
//! counts are bytes, not tokens. No tokenizer runs offline, so the share this
//! suite reports is not the token-denominated cache ratio, which only a live
//! endpoint can produce.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use houyicoder_api::provider::ModelProvider;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{ModelCatalogResolver, Runner, StubTool, ToolRegistry};
use houyicoder_memory::{InMemoryBackend, LocalFileBackend};
use houyicoder_protocol::cache_policy::PromptCacheFormat;
use houyicoder_protocol::llm::{EffortLevel, Usage, UsageProvenance};
use houyicoder_provider::OpenAiCompatibleProvider;
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;
use serde_json::{Value, json};
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::path::PathBuf;
use std::process::id;
use std::time::{SystemTime, UNIX_EPOCH};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Turns per session. The first request writes the entry the later turns read,
/// so the steady state is every turn after it.
const TURNS: usize = 6;

/// One request as the stand-in endpoint received it.
#[derive(Clone)]
struct RequestRecord {
    /// The cache key the body carried, absent when the request asked for none.
    cache_key: Option<String>,
    /// The body in full, so a turn that breaks the prefix can be compared with
    /// the turn before it at the byte where they first differ.
    body: Vec<u8>,
    /// Leading bytes shared with the body already held under the same key.
    shared: usize,
}

/// A stand-in endpoint for the caching contract: one cached entry per cache
/// key, and a reply that reports the shared leading bytes as its cache read.
struct PrefixCacheStub {
    entries: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    records: Arc<Mutex<Vec<RequestRecord>>>,
    reports_cache: bool,
    script_tool: bool,
    calls: Arc<AtomicU32>,
}

impl Clone for PrefixCacheStub {
    fn clone(&self) -> Self {
        Self {
            entries: Arc::clone(&self.entries),
            records: Arc::clone(&self.records),
            reports_cache: self.reports_cache,
            script_tool: self.script_tool,
            calls: Arc::clone(&self.calls),
        }
    }
}

impl PrefixCacheStub {
    /// A stand-in whose replies report the cache read.
    fn reporting() -> Self {
        Self::with_cache_account(true, false)
    }

    /// A stand-in that answers the first call with a tool call whose output is
    /// large enough to be externalized, then reports the cache read.
    fn reporting_tool() -> Self {
        Self::with_cache_account(true, true)
    }

    /// A stand-in whose replies say nothing about the cache, which leaves the
    /// cache read of every call unknown.
    fn silent() -> Self {
        Self::with_cache_account(false, false)
    }

    fn with_cache_account(reports_cache: bool, script_tool: bool) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            records: Arc::new(Mutex::new(Vec::new())),
            reports_cache,
            script_tool,
            calls: Arc::new(AtomicU32::new(0)),
        }
    }

    /// The requests received so far, in order.
    fn records(&self) -> Vec<RequestRecord> {
        self.records.lock().expect("records mutex").clone()
    }
}

impl Respond for PrefixCacheStub {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let body = request.body.clone();
        let cache_key = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|parsed| {
                parsed
                    .get("prompt_cache_key")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let held = cache_key.as_ref().and_then(|key| {
            self.entries
                .lock()
                .expect("entries mutex")
                .get(key)
                .cloned()
        });
        let shared = held
            .as_deref()
            .map_or(0, |prefix| shared_prefix(prefix, &body));
        let input = body.len();
        if let Some(key) = &cache_key {
            self.entries
                .lock()
                .expect("entries mutex")
                .insert(key.clone(), body.clone());
        }
        self.records
            .lock()
            .expect("records mutex")
            .push(RequestRecord {
                cache_key,
                body,
                shared,
            });
        let reply = if self.script_tool && call == 0 {
            tool_reply_body(shared, input)
        } else {
            reply_body(self.reports_cache, shared, input)
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(reply)
    }
}

/// How many leading bytes two bodies share. The count doubles as the offset of
/// the byte where they first differ.
fn shared_prefix(previous: &[u8], current: &[u8]) -> usize {
    previous
        .iter()
        .zip(current)
        .take_while(|(a, b)| a == b)
        .count()
}

/// The message list of a request body, serialized on its own.
///
/// A body frames the list with the fields that follow it, and the last of those
/// fields differs from the byte that closes the list, so the run of bytes two
/// consecutive requests share always stops inside the list. Comparing the list
/// apart from its framing keeps the check on the bytes the client sends for the
/// messages, without assuming where the framing sits.
fn messages_bytes(body: &[u8]) -> Vec<u8> {
    let parsed: Value = serde_json::from_slice(body).expect("a request body is json");
    serde_json::to_vec(&parsed["messages"]).expect("a message list serializes")
}

/// Everything a request body carries besides the message list, serialized on
/// its own: the model, the cache key, the streaming flags, and the tool list.
///
/// These fields serialize after the list, past the byte where two consecutive
/// bodies diverge, so the shared run never reaches them and a byte-prefix
/// comparison cannot say whether they held still. Compared apart, a tool entry
/// that changes from one turn to the next is caught.
fn framing_bytes(body: &[u8]) -> Vec<u8> {
    let mut parsed: Value = serde_json::from_slice(body).expect("a request body is json");
    if let Some(fields) = parsed.as_object_mut() {
        fields.remove("messages");
    }
    serde_json::to_vec(&parsed).expect("the fields around a message list serialize")
}

/// Where a request body carries cache markers: the total count, and whether
/// the system message, the last tool definition, and the newest message each
/// carry one.
fn marker_sites(body: &[u8]) -> (usize, bool, bool, bool) {
    let parsed: Value = serde_json::from_slice(body).expect("a request body is json");
    let total = parsed.to_string().matches("cache_control").count();
    let system = parsed["messages"][0]["content"][0]["cache_control"].is_object();
    let last_tool = parsed["tools"]
        .as_array()
        .and_then(|tools| tools.last())
        .is_some_and(|tool| tool["cache_control"].is_object());
    let tail = parsed["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .is_some_and(|message| message["content"][0]["cache_control"].is_object());
    (total, system, last_tool, tail)
}

/// The message list as it stands without the cache markers: a text part
/// carrying one collapses back to its plain string, which is what the same
/// message holds on a turn that marks another position.
///
/// The endpoint drops this control metadata before matching a prefix, so the
/// stripped form is the sequence it compares. The raw bytes of a
/// marker-bearing request are not: the marker moves to the newest message each
/// turn, so the message that carried it the turn before is rebuilt plain and
/// the two bodies differ there.
fn unmarked_messages(body: &[u8]) -> Vec<u8> {
    let mut parsed: Value = serde_json::from_slice(body).expect("a request body is json");
    let Some(Value::Array(mut messages)) = parsed.get_mut("messages").map(Value::take) else {
        panic!("a request body carries a message list");
    };
    for message in &mut messages {
        let plain = message
            .get("content")
            .and_then(Value::as_array)
            .and_then(|parts| parts.first())
            .and_then(|part| part.get("text"))
            .cloned();
        if let Some(text) = plain
            && let Some(content) = message.get_mut("content")
        {
            *content = text;
        }
    }
    serde_json::to_vec(&messages).expect("a message list serializes")
}

/// A streaming reply shaped like the endpoint's: content, the finish reason,
/// then a trailing chunk carrying usage. A stand-in that reports no cache read
/// leaves the detail object out, which is what an endpoint says when it has
/// nothing to report.
fn reply_body(reports_cache: bool, cached: usize, input: usize) -> String {
    let usage = if reports_cache {
        json!({
            "prompt_tokens": input,
            "completion_tokens": 1,
            "total_tokens": input + 1,
            "prompt_tokens_details": {"cached_tokens": cached},
        })
    } else {
        json!({
            "prompt_tokens": input,
            "completion_tokens": 1,
            "total_tokens": input + 1,
        })
    };
    let chunks = [
        json!({"choices": [{"index": 0, "delta": {"content": "ack"}, "finish_reason": null}], "usage": null}),
        json!({"choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "stop"}], "usage": null}),
        json!({"choices": [], "usage": usage}),
        json!("[DONE]"),
    ];
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect()
}

/// A reply that asks for the echo tool with an input large enough that the
/// result is externalized into a block_ref.
fn tool_reply_body(cached: usize, input: usize) -> String {
    let usage = json!({
        "prompt_tokens": input,
        "completion_tokens": 1,
        "total_tokens": input + 1,
        "prompt_tokens_details": {"cached_tokens": cached},
    });
    let arguments = json!({"data": "x".repeat(9_000)}).to_string();
    let chunks = [
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "echo", "arguments": arguments}}]}, "finish_reason": null}], "usage": null}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": null}),
        json!({"choices": [], "usage": usage}),
        json!("[DONE]"),
    ];
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect()
}

/// A catalog that declares the marker-bearing convention, so the request the
/// loop assembles carries it.
struct DeclaredMarkers;

impl ModelCatalogResolver for DeclaredMarkers {
    fn catalog_effort(&self, _model: &str) -> Option<EffortLevel> {
        None
    }

    fn catalog_cache_format(&self, _model: &str) -> Option<PromptCacheFormat> {
        Some(PromptCacheFormat::PositionalMarkers)
    }
}

/// Drive one session of short-tail turns against the stand-in and return the
/// runner, the session, and the usage each turn resolved.
///
/// The prompts ask for a bare word, so the loop completes on text and the next
/// request differs from this one by the appended exchange alone. A catalog
/// passed here is the endpoint's declaration; without one the requests keep
/// the undeclared form.
async fn drive(
    server: &MockServer,
    catalog: Option<Arc<dyn ModelCatalogResolver>>,
) -> (Runner, SessionId, Vec<Usage>) {
    let provider = Arc::new(OpenAiCompatibleProvider::new(
        server.uri(),
        "test-key".into(),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StubTool::new("echo")));
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        tools,
        RunnerConfig {
            model: "test-model".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    );
    let runner = match catalog {
        Some(resolver) => runner.with_catalog_resolver(resolver),
        None => runner,
    };
    let session = SessionId::new();
    let mut usages = Vec::new();
    for turn in 1..=TURNS {
        let prompt = format!("Reply with the single word ack. This is message {turn}.");
        let result = runner
            .run(session, prompt)
            .await
            .unwrap_or_else(|error| panic!("turn {turn} failed: {error:?}"));
        usages.push(result.usage);
    }
    (runner, session, usages)
}

/// One usage row as the durable log holds it.
struct UsageRow {
    /// The inclusive input total the endpoint reported.
    input: u64,
    /// The cache-read count the endpoint reported.
    cached: u64,
    /// Whether the endpoint reported both counts, which is what lets a reader
    /// divide one by the other.
    complete: bool,
}

/// The usage rows one session recorded, in order.
fn rows(runner: &Runner, session: SessionId) -> Vec<UsageRow> {
    runner
        .store()
        .trajectory_tail(session)
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::TurnUsage {
                input_tokens,
                cache_read_input_tokens,
                input_reported,
                cache_read_reported,
                cache_write_reported,
                ..
            } => Some(UsageRow {
                input: input_tokens,
                cached: cache_read_input_tokens,
                complete: UsageProvenance {
                    input_reported,
                    cache_read_reported,
                    cache_write_reported,
                }
                .cache_account_complete(),
            }),
            _ => None,
        })
        .collect()
}

/// Print what each request shared with the one before it, then the share over
/// the steady state. The rows are the diagnostic when a later turn breaks the
/// prefix.
fn report(records: &[RequestRecord]) {
    println!(
        "turn 1 request {} bytes, writes the entry the later turns read",
        records[0].body.len()
    );
    let mut shared = 0usize;
    let mut input = 0usize;
    for (index, record) in records.iter().enumerate().skip(1) {
        let share = record.shared as f64 * 100.0 / record.body.len() as f64;
        println!(
            "turn {:<3} request {:<7} shared {:<7} = {share:.1}% of that request body",
            index + 1,
            record.body.len(),
            record.shared
        );
        shared += record.shared;
        input += record.body.len();
    }
    let ratio = shared as f64 * 100.0 / input as f64;
    println!(
        "prefix reuse: {shared} shared / {input} request bytes = {ratio:.2}% over {} steady-state turns",
        records.len() - 1
    );
}

/// A session whose turns only append an exchange must send a message list that
/// grows by that exchange alone, under one unchanged cache key, so the run of
/// bytes the endpoint can reuse grows every turn. Every value the client
/// renders into that list — the assembled identity and the project rules —
/// has to hold still for that to be true, so a value that drifts per turn is
/// caught here, and the fields around the list are checked to hold still the
/// same way.
#[tokio::test]
async fn test_turns_extend_request_prefix() {
    let server = MockServer::start().await;
    let stub = PrefixCacheStub::reporting();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(stub.clone())
        .mount(&server)
        .await;

    let (_runner, _session, _usages) = drive(&server, None).await;
    let records = stub.records();
    assert_eq!(records.len(), TURNS, "one request per turn");
    assert!(
        records.iter().all(|record| record.cache_key.is_some()),
        "every request carries a cache key: {:?}",
        records
            .iter()
            .map(|record| record.cache_key.as_deref())
            .collect::<Vec<_>>()
    );
    assert!(
        records
            .iter()
            .all(|record| record.cache_key == records[0].cache_key),
        "one cache key for the whole session, got {:?}",
        records
            .iter()
            .map(|record| record.cache_key.as_deref())
            .collect::<Vec<_>>()
    );
    report(&records);
    assert_eq!(
        records[0].shared, 0,
        "the first request has no entry to read"
    );
    let mut reusable = Vec::new();
    for (index, pair) in records.windows(2).enumerate() {
        let (before, after) = (&pair[0], &pair[1]);
        let shared = shared_prefix(&before.body, &after.body);
        reusable.push(shared);
        let previous = messages_bytes(&before.body);
        let current = messages_bytes(&after.body);
        assert!(
            previous.len() < current.len(),
            "turn {} appended nothing to the message list",
            index + 2
        );
        assert!(
            framing_bytes(&before.body) == framing_bytes(&after.body),
            "turn {} rewrote a field outside the message list, so a value rendered there changes \
             from one turn to the next",
            index + 2
        );
        // The closing bracket of the previous list gives way to the exchange
        // this turn appended, so everything the list held is still there.
        let head = &previous[..previous.len() - 1];
        assert!(
            current.starts_with(head),
            "turn {} rewrote the message list it should only extend: the two first differ at byte \
             {}",
            index + 2,
            shared_prefix(head, &current)
        );
        assert!(
            shared >= previous.len() - 1,
            "turn {} and turn {} share {shared} bytes, short of the {} the previous message list \
             contributes",
            index + 1,
            index + 2,
            previous.len() - 1
        );
    }
    for (index, pair) in reusable.windows(2).enumerate() {
        assert!(
            pair[1] > pair[0],
            "the run of bytes turns {} and {} share ({}) does not grow over the run turns {} and {} \
             share ({})",
            index + 2,
            index + 3,
            pair[1],
            index + 1,
            index + 2,
            pair[0]
        );
    }
}

/// A session on an endpoint that declares the marker-bearing convention sends
/// one marker at each placed breakpoint — the system prefix, the last tool
/// definition, the newest message — and no cache label, and the sequence the
/// endpoint matches on still grows by the appended exchange alone once the
/// markers are stripped.
///
/// The stand-in compares raw bytes under a cache key this form does not send,
/// so the reuse it reports here is a lower bound rather than a share to read.
#[tokio::test]
async fn test_markers_hold_positions() {
    let server = MockServer::start().await;
    let stub = PrefixCacheStub::reporting();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(stub.clone())
        .mount(&server)
        .await;

    let (_runner, _session, _usages) = drive(&server, Some(Arc::new(DeclaredMarkers))).await;
    let records = stub.records();
    assert_eq!(records.len(), TURNS, "one request per turn");
    for (index, record) in records.iter().enumerate() {
        assert!(
            record.cache_key.is_none(),
            "turn {} sent a cache label the marker form does not use",
            index + 1
        );
        let sites = marker_sites(&record.body);
        assert_eq!(
            sites,
            (3, true, true, true),
            "turn {} markers: {}",
            index + 1,
            String::from_utf8_lossy(&record.body)
        );
    }

    let mut reusable = Vec::new();
    for (index, pair) in records.windows(2).enumerate() {
        let (before, after) = (&pair[0], &pair[1]);
        let previous = unmarked_messages(&before.body);
        let current = unmarked_messages(&after.body);
        assert!(
            previous.len() < current.len(),
            "turn {} appended nothing to the message list",
            index + 2
        );
        // The closing bracket of the previous list gives way to the exchange
        // this turn appended, so everything the list held is still there.
        let head = &previous[..previous.len() - 1];
        assert!(
            current.starts_with(head),
            "turn {} rewrote the message list it should only extend: the two first differ at byte \
             {}",
            index + 2,
            shared_prefix(head, &current)
        );
        reusable.push(shared_prefix(&previous, &current));
    }
    for (index, pair) in reusable.windows(2).enumerate() {
        assert!(
            pair[1] > pair[0],
            "the run of unmarked bytes turns {} and {} share ({}) does not grow over the run turns \
             {} and {} share ({})",
            index + 2,
            index + 3,
            pair[1],
            index + 1,
            index + 2,
            pair[0]
        );
    }
}

/// What the endpoint reported has to arrive in the durable rows intact, so the
/// share a pane derives from the log is the share the endpoint's bytes support.
/// The two sides are computed apart: the stand-in counts bytes as it answers,
/// the log holds what came back over the wire.
#[tokio::test]
async fn test_cache_report_reaches_log() {
    let server = MockServer::start().await;
    let stub = PrefixCacheStub::reporting();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(stub.clone())
        .mount(&server)
        .await;

    let (runner, session, _usages) = drive(&server, None).await;
    let records = stub.records();
    let logged = rows(&runner, session);
    assert_eq!(logged.len(), records.len(), "one usage row per request");
    for (index, (row, record)) in logged.iter().zip(&records).enumerate() {
        assert_eq!(
            row.input,
            record.body.len() as u64,
            "turn {} logged an input count the endpoint did not report",
            index + 1
        );
        assert_eq!(
            row.cached,
            record.shared as u64,
            "turn {} logged a cache read the endpoint did not report",
            index + 1
        );
        assert!(
            row.complete,
            "turn {} logged an incomplete account",
            index + 1
        );
    }
    let from_rows = logged.iter().skip(1).fold((0u64, 0u64), |sum, row| {
        (sum.0 + row.cached, sum.1 + row.input)
    });
    let from_bytes = records
        .iter()
        .skip(1)
        .fold((0usize, 0usize), |sum, record| {
            (sum.0 + record.shared, sum.1 + record.body.len())
        });
    assert_eq!(
        from_rows,
        (from_bytes.0 as u64, from_bytes.1 as u64),
        "the read total and the cache total of the log diverge from the endpoint's own accounting"
    );
}

/// An endpoint that says nothing about the cache leaves the share unknown. The
/// rows keep the input count it did report, claim no cache read, and mark the
/// account incomplete, so no reader can divide the silence into a 0% that reads
/// like a miss.
#[tokio::test]
async fn test_absent_account_stays_unknown() {
    let server = MockServer::start().await;
    let stub = PrefixCacheStub::silent();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(stub.clone())
        .mount(&server)
        .await;

    let (runner, session, usages) = drive(&server, None).await;
    let logged = rows(&runner, session);
    assert_eq!(logged.len(), TURNS, "one usage row per turn");
    for (index, row) in logged.iter().enumerate() {
        assert!(row.input > 0, "turn {} reported no input total", index + 1);
        assert_eq!(row.cached, 0, "turn {} claimed a cache read", index + 1);
        assert!(
            !row.complete,
            "turn {} was logged as a complete account",
            index + 1
        );
    }
    assert!(
        usages.iter().all(|usage| usage.cache_hit_rate().is_none()),
        "a usage whose endpoint reported no cache read yields no share: {:?}",
        usages.iter().map(Usage::cache_hit_rate).collect::<Vec<_>>()
    );
}

/// A reopened process sends the same message prefix as the process that wrote
/// the log, including a block_ref old enough to have been frozen at
/// Materialize. The comparison is the parsed messages array, element by
/// element, plus the tool definitions.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one writer-then-reopen journey; the store, server, and runner are the fixture"
)]
async fn test_resume_keeps_prefix() {
    let server = MockServer::start().await;
    let stub = PrefixCacheStub::reporting_tool();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(stub.clone())
        .mount(&server)
        .await;

    let root: PathBuf = temp_dir().join(format!("cache-resume-{}", id()));
    create_dir_all(&root).expect("temp root");
    let provider: Arc<dyn ModelProvider> = Arc::new(OpenAiCompatibleProvider::new(
        server.uri(),
        "test-key".into(),
    ));
    let config = || RunnerConfig {
        model: "test-model".into(),
        instructions: String::new(),
        max_turns: 5,
        max_output_tokens: 8_000,
        retry: Retry {
            max_attempts: 2,
            ..Retry::default()
        },
    };
    let tools = || {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(StubTool::new("echo")));
        tools
    };
    let session = SessionId::new();
    let store_a = Arc::new(SessionStore::new(Box::new(LocalFileBackend::new(
        root.clone(),
    ))));
    // Start the session three days ago: the resumed process must freeze the
    // same date line the writer's epoch produced, not the reopen date.
    let old = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
        .saturating_sub(3 * 86_400_000);
    store_a
        .append(SessionLogEntry {
            id: EventId::new(),
            session,
            ts: old,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "older".into(),
            },
        })
        .await
        .expect("the backdated event");
    let runner_a = Runner::new(store_a, Arc::clone(&provider), tools(), config());
    runner_a.seed_session_from_log(session);
    for turn in 1..=10 {
        runner_a
            .run(
                session,
                format!("Reply with the single word ack. This is message {turn}."),
            )
            .await
            .unwrap_or_else(|error| panic!("turn {turn} failed: {error:?}"));
    }
    let records = stub.records();
    let a_count = records.len();
    let a_last = records
        .last()
        .expect("the writer sent a request")
        .body
        .clone();

    let store_b = Arc::new(SessionStore::new(Box::new(LocalFileBackend::new(
        root.clone(),
    ))));
    let runner_b = Runner::new(store_b, provider, tools(), config());
    runner_b.seed_session_from_log(session);
    runner_b
        .run(
            session,
            "Reply with the single word ack. This is message 11.".into(),
        )
        .await
        .expect("the resumed turn");
    let records = stub.records();
    let b_first = records
        .get(a_count)
        .expect("the reopened process sent a request")
        .body
        .clone();

    let a_json: Value = serde_json::from_slice(&a_last).expect("writer request json");
    let b_json: Value = serde_json::from_slice(&b_first).expect("resume request json");
    let a_messages = a_json["messages"].as_array().expect("writer messages");
    let b_messages = b_json["messages"].as_array().expect("resume messages");
    assert!(
        b_messages.len() > a_messages.len(),
        "the resumed request appends the new exchange"
    );
    assert_eq!(
        &b_messages[..a_messages.len()],
        a_messages.as_slice(),
        "the resumed request keeps every previous message byte for byte"
    );
    assert_eq!(
        a_json["tools"], b_json["tools"],
        "the tool definitions stay identical"
    );
    remove_dir_all(&root).ok();
}
