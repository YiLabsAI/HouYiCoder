//! Integration test: the agent loop wires BashTool + a real macOS Seatbelt
//! sandbox + HITL approval end to end. Exercises Runner run/resume against
//! MacSeatbeltSession (real sandbox-exec), so it lives outside the unit suite.
//! Run with: make test integration. Mac-only.

#![cfg(target_os = "macos")]

use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, ModelCapabilities, OutputItem, ProviderError,
};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;

use houyicoder_api::provider::stream_from_response;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{ApprovalDecision, BashTool, RunOutcome, Runner, ToolRegistry};
use houyicoder_protocol::llm::LlmEvent;
use houyicoder_sandbox::MacSeatbeltSession;
use std::sync::Mutex;
use std::time::Duration;
use std::vec::IntoIter;
use tokio::time::sleep;

/// A provider that returns scripted responses in sequence, then repeats the
/// last one forever. Test-only helper re-implemented here because integration
/// tests reach the crate only through its public API.
struct FakeProvider {
    responses: Mutex<Option<IntoIter<CompletionResponse>>>,
    last: Mutex<Option<CompletionResponse>>,
}

impl FakeProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(Some(responses.into_iter())),
            last: Mutex::new(None),
        }
    }
}

impl ModelProvider for FakeProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let next = {
            let mut iter_guard = self.responses.lock().expect("script mutex");
            if let Some(iter) = iter_guard.as_mut() {
                if let Some(next) = iter.next() {
                    *self.last.lock().expect("last mutex") = Some(next.clone());
                    Some(next)
                } else {
                    self.last.lock().expect("last mutex").clone()
                }
            } else {
                None
            }
        };
        let next = next.expect("script has no responses");
        Box::pin(async move { Ok(next) })
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        let next = {
            let mut iter_guard = self.responses.lock().expect("script mutex");
            if let Some(iter) = iter_guard.as_mut() {
                if let Some(next) = iter.next() {
                    *self.last.lock().expect("last mutex") = Some(next.clone());
                    Some(next)
                } else {
                    self.last.lock().expect("last mutex").clone()
                }
            } else {
                None
            }
        };
        stream_from_response(next.expect("script has no responses"))
    }
}

#[tokio::test]
async fn test_loop_bash() {
    let responses = vec![
        CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "let me run".into(),
                },
                OutputItem::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo integrated-loop > log.txt"}),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let provider = Arc::new(FakeProvider::new(responses));
    let session: Arc<dyn SandboxSession> = Arc::new(MacSeatbeltSession::new().expect("seatbelt"));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BashTool::new(session.clone())));
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    );
    let sid = SessionId::new();

    let result = runner.run(sid, "run bash".into()).await.expect("run");
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected interruption, got {other:?}"),
    };
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].tool_name, "bash");
    let decisions = approvals
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect::<Vec<_>>();

    let resumed = runner.resume(sid, &decisions).await.expect("resume");
    match resumed.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "done"),
        other => panic!("expected final output, got {other:?}"),
    }
    // The sandbox really ran the command: log.txt exists with our text.
    let log =
        String::from_utf8(session.read_file("log.txt", 64).await.expect("read")).expect("utf8");
    assert_eq!(log.trim(), "integrated-loop");
}

/// Poll the sandbox workspace until the command has created its marker. The
/// budget covers a cold sandbox launch; the marker appears as soon as the
/// shell starts, so the abort lands while the command still runs.
async fn wait_for_file(session: &Arc<dyn SandboxSession>, path: &str) {
    for _ in 0..200 {
        if session.read_file(path, 64).await.is_ok() {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("the sandboxed command never created {path}");
}

/// Whether a process id still names a live process, or None when the probe
/// itself could not run. A test observation of the machine, not an engine
/// process launch, so it may call ps directly. The caller must tell a process
/// that is gone from a probe that never ran: reading a failed probe as
/// absence would let the assertion below pass without observing anything.
#[allow(clippy::disallowed_methods)]
fn process_alive(pid: u32) -> Option<bool> {
    Command::new("ps")
        .args(["-p", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|status| status.success())
}

/// Abort during a running sandboxed command must kill the command itself,
/// not only stop waiting for it. The command reports the shell's process id
/// and writes a start marker, sleeps, then writes a second marker. Once the
/// abort lands, the shell must be gone and the second marker must never
/// appear: a cancel that only dropped the tool future would leave the shell
/// alive to finish its sleep and write that marker. The marker is checked
/// after the shell is gone, so a surviving shell has already had its whole
/// sleep to write it.
#[tokio::test]
async fn test_abort_kills_bash() {
    let responses = vec![CompletionResponse {
        output: vec![
            OutputItem::Text {
                text: "start a slow command".into(),
            },
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({
                    "command": "echo $$ > abort-pid.txt; touch abort-start.txt; sleep 1.2; touch abort-finish.txt"
                }),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    }];
    let provider = Arc::new(FakeProvider::new(responses));
    let session: Arc<dyn SandboxSession> = Arc::new(MacSeatbeltSession::new().expect("seatbelt"));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BashTool::new(session.clone())));
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    );
    let sid = SessionId::new();

    let result = runner
        .run(sid, "run the slow command".into())
        .await
        .expect("run");
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected an approval ask, got {other:?}"),
    };
    let decisions = approvals
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect::<Vec<_>>();

    let runner = Arc::new(runner);
    let worker = Arc::clone(&runner);
    let resumed = tokio::spawn(async move { worker.resume(sid, &decisions).await });
    wait_for_file(&session, "abort-pid.txt").await;
    let pid = String::from_utf8(
        session
            .read_file("abort-pid.txt", 64)
            .await
            .expect("read pid"),
    )
    .expect("utf8")
    .trim()
    .parse::<u32>()
    .expect("the command reports its shell's process id");
    wait_for_file(&session, "abort-start.txt").await;
    runner.abort();

    let outcome = resumed.await.expect("resume task").expect("resume").outcome;
    assert!(
        matches!(outcome, RunOutcome::Interrupted(_)),
        "abort ends the run as interrupted, got {outcome:?}"
    );

    // The kill may land just after the run returns; wait for the shell to be
    // gone. The budget covers the command's own sleep, so a shell that
    // survived the abort exits inside it and is caught below by its marker.
    let mut gone = false;
    for _ in 0..150 {
        match process_alive(pid) {
            Some(false) => {
                gone = true;
                break;
            }
            // A probe that did not run is retried rather than read as the
            // shell having exited, so the loop cannot end on no observation.
            Some(true) | None => sleep(Duration::from_millis(20)).await,
        }
    }
    assert!(
        gone,
        "the cancelled command's shell {pid} is still running or was never observed"
    );
    // An absent marker means something only if this read channel answers:
    // checked on the marker the command already wrote, so a read error cannot
    // stand in for the command having stopped.
    session
        .read_file("abort-start.txt", 64)
        .await
        .expect("the read channel still answers after the abort");
    assert!(
        session.read_file("abort-finish.txt", 64).await.is_err(),
        "the cancelled command did not run past the sleep"
    );
}
