//! Tests for this module.

use super::*;
use houyicoder_api::provider::stream_from_response;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{EventId, MemoryEntry, MemorySource, SessionEvent, SessionLogEntry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_session::SessionStore;
use std::sync::Mutex;

use crate::agent::{MemoryRuntime, RunOutcome};
use crate::provider::test_support::FakeProvider;

/// A memory provider that records every add so the test can assert the
/// forked agent wrote through the shared provider.
struct RecordingMemory {
    written: Mutex<Vec<MemoryEntry>>,
}

impl RecordingMemory {
    fn written_entries(&self) -> Vec<MemoryEntry> {
        self.written.lock().expect("written").clone()
    }
}

impl MemoryProvider for RecordingMemory {
    fn add(&self, entry: MemoryEntry) -> Result<(), houyicoder_context::MemoryError> {
        self.written.lock().expect("written").push(entry);
        Ok(())
    }
}

/// A scripted provider: call 1 emits a save_memory tool call (the forked
/// agent extracting a fact), call 2 emits a final text response so the
/// loop ends within the max_turns budget. Records the instructions plus
/// assembled input so the test can assert both reached the model.
struct ForkedAgentProvider {
    calls: Mutex<usize>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl ModelProvider for ForkedAgentProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let mut calls = self.calls.lock().expect("calls");
        *calls += 1;
        let n = *calls;
        drop(calls);
        self.seen.lock().expect("seen").push(capture(&req));
        let resp = scripted_response(n);
        Box::pin(async move { Ok(resp) })
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let mut calls = self.calls.lock().expect("calls");
        *calls += 1;
        let n = *calls;
        drop(calls);
        self.seen.lock().expect("seen").push(capture(&req));
        stream_from_response(scripted_response(n))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Capture instructions plus assembled input as one string so the test
/// asserts both the system prompt (tool docs) and the prefix conversation
/// reached the forked agent.
fn capture(req: &CompletionRequest) -> String {
    let input = serde_json::to_string(&req.input).unwrap_or_default();
    format!("{}\n<<<INPUT>>>\n{input}", req.instructions)
}

/// Turn-1 save-memory call + turn-2 final text.
fn scripted_response(n: usize) -> CompletionResponse {
    if n == 1 {
        CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "Saving a user preference.".into(),
                },
                OutputItem::ToolCall {
                    id: "save1".into(),
                    name: "save_memory".into(),
                    input: serde_json::json!({
                        "key": "user-prefers-terse",
                        "description": "User prefers terse responses",
                        "source": "feedback",
                        "content": "Keep responses terse.\n**Why:** the user said long intros waste their time.\n**How to apply:** drop preamble, lead with the answer.",
                        "evidence": [{"quote": "the long intros waste my time"}]
                    }),
                },
            ],
            usage: Usage::default(),
            model: "test".to_string(),
        }
    } else {
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".to_string(),
        }
    }
}

/// A minimal main-session prefix: a user asks for terse responses, the
/// assistant agrees. The forked extraction agent distills the feedback.
fn main_prefix() -> Vec<SessionLogEntry> {
    let session = SessionId::new();
    vec![
        SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "Please keep your responses terse, the long intros waste my time.".into(),
            },
        },
        SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::AssistantMessage {
                text: "Got it — I will lead with the answer and drop the preamble.".into(),
                thinking: None,
            },
        },
    ]
}

fn config() -> RunnerConfig {
    RunnerConfig {
        max_turns: 5,
        ..RunnerConfig::default()
    }
}

/// The forked agent emits a structured save-memory call that lands
/// through the shared memory provider. This is the core assertion: the
/// forked extraction run autonomously writes a memory from the prefix.
#[tokio::test]
async fn test_forked_extract_writes_memory() {
    let memory = Arc::new(RecordingMemory {
        written: Mutex::new(Vec::new()),
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(ForkedAgentProvider {
        calls: Mutex::new(0),
        seen: seen.clone(),
    });
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));

    let cwd = temp_dir().join(format!("fork-extract-{}", id()));
    create_dir_all(&cwd).expect("mkdir cwd");

    let prefix = main_prefix();
    let recorder = Arc::new(MutationLog::new());
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&prefix, None).expect("fresh cursor locates"),
    )
    .expect("a turn is present");
    let result = run_forked_extract(
        ephemeral,
        provider,
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        &cwd,
        config(),
        &window,
        recorder,
    )
    .await
    .expect("forked run must not error");

    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "done"),
        "forked run must reach final output"
    );

    let written = memory.written_entries();
    assert_eq!(written.len(), 1, "exactly one memory must be written");
    assert_eq!(written[0].key, "user-prefers-terse");
    assert_eq!(written[0].source, MemorySource::Feedback);
    assert!(
        written[0].content.contains("Keep responses terse"),
        "body content landed"
    );

    // The prefix conversation + the extraction tool doc both reached the
    // forked agent: the user's terse ask is in the assembled input, the
    // save_memory tool doc is in the system instructions.
    let first = seen.lock().expect("seen")[0].clone();
    assert!(
        first.contains("terse"),
        "prefix conversation reached the forked agent: {first}"
    );
    assert!(
        first.contains("save_memory"),
        "extraction tool doc reached the forked agent"
    );

    remove_dir_all(&cwd).ok();
}

/// The run completes within max_turns for a longer prefix and still
/// writes exactly one memory. Guards against the prefix seed or the
/// drive loop regressing on a multi-event prefix.
#[tokio::test]
async fn test_forked_extract_clones_prefix() {
    let memory = Arc::new(RecordingMemory {
        written: Mutex::new(Vec::new()),
    });
    let provider = Arc::new(ForkedAgentProvider {
        calls: Mutex::new(0),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let cwd = temp_dir().join(format!("fork-prefix-{}", id()));
    create_dir_all(&cwd).expect("mkdir cwd");
    let prefix = main_prefix();
    let recorder = Arc::new(MutationLog::new());
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&prefix, None).expect("fresh cursor locates"),
    )
    .expect("a turn is present");
    let result = run_forked_extract(
        ephemeral,
        provider,
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        &cwd,
        config(),
        &window,
        recorder,
    )
    .await
    .expect("forked run must not error");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    assert_eq!(
        memory.written_entries().len(),
        1,
        "one memory written from the prefix"
    );
    remove_dir_all(&cwd).ok();
}

/// install_memory registers the memory tool set on the runner: the write
/// tool plus the two read-only tools an agent needs to find a memory again
/// and read one body back. A read tool registered only on the dream fork
/// leaves the main agent unable to inspect what recall surfaced.
#[test]
fn test_install_memory_registers_tool() {
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let memory = Arc::new(RecordingMemory {
        written: Mutex::new(Vec::new()),
    });
    let mut runtime = MemoryRuntime::new(store.clone());
    runtime.install_provider(Arc::clone(&memory) as Arc<dyn MemoryProvider>);
    let runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .install_memory(runtime);
    for tool in ["save_memory", "search_memory", "show_memory"] {
        assert!(
            runner.tools().get(tool).is_some(),
            "install_memory must register {tool}"
        );
    }
    assert!(
        memory.written_entries().is_empty(),
        "no write until the agent emits a save_memory call"
    );
}

/// The forked extraction runner's save_memory tool exposes no scope
/// field: where an extraction write lands is the host's decision.
#[test]
fn test_extract_tool_hides_scope() {
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let memory = Arc::new(RecordingMemory {
        written: Mutex::new(Vec::new()),
    });
    let runner = build_forked_extract_runner(
        store,
        provider,
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        Path::new("."),
        config(),
        Arc::new(MutationLog::new()),
        Arc::from(Vec::new()),
    );
    let tool = runner
        .tools()
        .get("save_memory")
        .expect("extraction registers save_memory");
    let schema = tool.input_schema();
    let props = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("properties object");
    assert!(
        !props.contains_key("scope"),
        "the extraction schema must not offer a scope choice"
    );
}
