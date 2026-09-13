//! Runs bounded memory extraction in an ephemeral session.
//!
//! The model and memory provider are shared with the main runner, while the
//! conversation log remains isolated.

use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionId, SessionLogEntry};
use std::path::Path;
use std::sync::Arc;

use super::memory::MutationLog;
use super::prompt::extract::build_extraction_prompt;
use super::runner_config::RunnerConfig;
use super::{RunError, RunOutcome, RunResult, Runner, ToolRegistry};

/// Build a forked extraction runner over a caller-provided ephemeral store.
/// The store is ephemeral (an in-memory backend the caller constructs) so the
/// forked transcript stays out of the durable main log. The provider and
/// memory are shared (Arc clone) so prompt caching and the in-process write
/// lock carry over. The save_memory tool is registered with the given
/// recorder so the caller can drain the exact operations the fork landed
/// this pass (the main runner's tool has no recorder; it does not notify).
pub(crate) fn build_forked_extract_runner(
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: &Path,
    config: RunnerConfig,
    recorder: Arc<MutationLog>,
) -> Runner {
    let tools = ToolRegistry::new();
    Runner::new(store, provider, tools, config)
        .install_extraction_memory(memory, recorder)
        .with_cwd(cwd.to_path_buf())
}

/// The full cache prefix paired with its cursor-derived eligible suffix size.
pub(crate) struct ExtractionWindow<'a> {
    pub(crate) prefix: &'a [SessionLogEntry],
    pub(crate) new_message_count: usize,
}

/// Drive a forked extraction run on a fresh ephemeral session. The full
/// conversation remains the cache prefix while the window count limits which
/// trailing messages may produce writes.
pub(crate) async fn run_forked_extract(
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: &Path,
    config: RunnerConfig,
    window: ExtractionWindow<'_>,
    recorder: Arc<MutationLog>,
) -> Result<RunResult, RunError> {
    // Inject the existing-memory manifest so the forked agent dedups by
    // reusing a key instead of re-saving the same fact each turn (a
    // formatMemoryManifest pre-inject). Built before moving memory into
    // the runner.
    let prompt = build_extraction_prompt(window.new_message_count, &memory.list_memories());
    let runner = build_forked_extract_runner(store, provider, memory, cwd, config, recorder);
    let session = SessionId::new();
    let result = runner.run_forked(session, window.prefix, prompt).await;
    // A fork that hits the turn cap did not finish extracting — treat as a
    // failure so the extractor's cursor stays and the range is reconsidered
    // next pass. The main loop's max_turns is a graceful RunOutcome; a
    // fork's is a failure (it did not finish the extraction pass).
    match result {
        Ok(RunResult {
            outcome: RunOutcome::MaxTurnsReached { turns },
            ..
        }) => Err(RunError::MaxTurnsExceeded { turns }),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_api::provider::stream_from_response;
    use houyicoder_async::{PFut, PStream};
    use houyicoder_context::{EventId, MemoryEntry, MemorySource, SessionEvent};
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_protocol::llm::{
        CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem,
        ProviderError, Usage,
    };
    use houyicoder_session::SessionStore;
    use std::collections::HashSet;
    use std::sync::Mutex;

    use crate::agent::RunOutcome;

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
        fn recall(
            &self,
            _query: &str,
            _budget: usize,
            _surfaced: &HashSet<String>,
        ) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn add(&self, entry: MemoryEntry) -> Result<(), houyicoder_context::MemoryError> {
            self.written.lock().expect("written").push(entry);
            Ok(())
        }
    }

    /// A scripted provider: call 1 emits a save_memory tool call (the forked
    /// agent extracting a fact), call 2 emits a final text response so the
    /// loop ends within the max_turns budget. Records the instructions plus
    /// projected input so the test can assert both reached the model.
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

    /// Capture instructions plus projected input as one string so the test
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
                            "content": "Keep responses terse.\n**Why:** the user said long intros waste their time.\n**How to apply:** drop preamble, lead with the answer."
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

        let cwd = std::env::temp_dir().join(format!("fork-extract-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");

        let prefix = main_prefix();
        let recorder = Arc::new(MutationLog::new());
        let result = run_forked_extract(
            ephemeral,
            provider,
            Arc::clone(&memory) as Arc<dyn MemoryProvider>,
            &cwd,
            config(),
            ExtractionWindow {
                prefix: &prefix,
                new_message_count: 2,
            },
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
        // forked agent: the user's terse ask is in the projected input, the
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

        std::fs::remove_dir_all(&cwd).ok();
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
        let cwd = std::env::temp_dir().join(format!("fork-prefix-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("mkdir cwd");
        let prefix = main_prefix();
        let recorder = Arc::new(MutationLog::new());
        let result = run_forked_extract(
            ephemeral,
            provider,
            Arc::clone(&memory) as Arc<dyn MemoryProvider>,
            &cwd,
            config(),
            ExtractionWindow {
                prefix: &prefix,
                new_message_count: 2,
            },
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
        std::fs::remove_dir_all(&cwd).ok();
    }

    /// install_memory registers the structured save_memory tool.
    #[test]
    fn test_install_memory_registers_tool() {
        use crate::agent::MemoryRuntime;
        let store: Arc<dyn SessionLog> =
            Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let provider: Arc<dyn ModelProvider> =
            Arc::new(crate::provider::test_support::FakeProvider::text("ok"));
        let memory = Arc::new(RecordingMemory {
            written: Mutex::new(Vec::new()),
        });
        let mut runtime = MemoryRuntime::new(store.clone());
        runtime.install_provider(Arc::clone(&memory) as Arc<dyn MemoryProvider>);
        let runner = Runner::with_shared_store(
            store,
            provider,
            ToolRegistry::new(),
            RunnerConfig::default(),
        )
        .install_memory(runtime);
        assert!(
            runner.tools().get("save_memory").is_some(),
            "install_memory must register the save_memory tool"
        );
        assert!(
            memory.written_entries().is_empty(),
            "no write until the agent emits a save_memory call"
        );
    }
}
