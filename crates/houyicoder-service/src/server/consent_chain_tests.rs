//! Full approval-to-execution consent tests for sandbox-backed tools. A real
//! runner, registry, sandbox session, and reverse request verify that approved
//! capabilities reach the final filesystem effect.

use super::*;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::{SandboxError, SessionId};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{EditTool, Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{
    AskReason, AskSource, Decision, DefaultModeGate, FileRuleStore, GuardedTool, ModeGate,
    RuleStore, ToolRequest,
};
use houyicoder_protocol::envelope::{
    ClientResponsePayload, RequestId, ResponsePayload, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, ContentBlock};
use houyicoder_protocol::frontend::{FrontendRequest, SessionId as WireSessionId};
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_provider::FakeProvider;
use houyicoder_sandbox::PlatformSession;
use houyicoder_session::SessionStore;

use crate::composition::SessionHost;
use crate::lifecycle::SessionLeaseStore;
use crate::server::EventSequencer;
use serde_json::json;
use std::{env, fs, path::Path, process, sync::Arc, time::Duration};

/// A runner over an in-memory session store with a fixed turn budget, the
/// shape both chain tests drive.
fn chain_runner(
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    sandbox: Option<Arc<dyn SandboxSession>>,
) -> Arc<Runner> {
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(
        Runner::new(
            sess_store,
            provider,
            tools,
            RunnerConfig {
                model: "test".into(),
                instructions: "test".into(),
                max_turns: 5,
                ..Default::default()
            },
        )
        .with_sandbox_session(sandbox),
    )
}

/// The provider pair for the edit chain: one completion calling edit on the
/// target, one ending the turn with text.
fn edit_completions(target: &Path) -> Vec<CompletionResponse> {
    let edit_input = json!({
        "path": target.to_string_lossy(),
        "old_string": "old",
        "new_string": "new",
    });
    vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "toolu_1".into(),
                name: "edit".into(),
                input: edit_input,
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "edited".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]
}

/// A runner whose registry guards EditTool with the same gate the server
/// uses, so the ask the client answers is the ask the tool produced.
fn guarded_edit_runner(
    sandbox: Arc<dyn SandboxSession>,
    gate: Arc<dyn ModeGate>,
    target: &Path,
) -> Arc<Runner> {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new(
        Arc::new(EditTool::new(sandbox.clone())),
        gate,
    )));
    chain_runner(
        Arc::new(FakeProvider::new(edit_completions(target))),
        tools,
        Some(sandbox),
    )
}

/// Drain frames until the final response, answering the single permission
/// ask with an approved once. Returns whether the run finished with RunOk.
async fn approve_once_and_finish(
    client: &mut Client,
    expected: RequestId,
    expected_directory: &Path,
) -> bool {
    for _ in 0..64 {
        let frame = tokio::time::timeout(Duration::from_secs(5), client.next_frame())
            .await
            .expect("server frame timeout")
            .expect("server frame");
        match frame {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                let (call_id, reason) = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest {
                        call_id, reason, ..
                    }) => (call_id, reason),
                    _ => panic!("expected a permission reverse request"),
                };
                let reason = reason.expect("the ask carries its reason");
                assert_eq!(reason.validator, "protected_path");
                assert!(
                    reason.detail.contains("read-write access")
                        && reason
                            .detail
                            .contains(&expected_directory.to_string_lossy().to_string()),
                    "the card discloses the parent capability: {}",
                    reason.detail
                );
                client
                    .send_reverse_response(
                        ask.req_id,
                        ClientResponsePayload::Permission(ApprovalDecision {
                            call_id,
                            approved: true,
                            updated_input: None,
                            scope: "once".to_string(),
                        }),
                    )
                    .await
                    .expect("answer reverse request");
            }
            ServerFrame::Response(resp) if resp.req_id == expected => match resp.payload {
                ResponsePayload::RunOk(_) => return true,
                other => panic!("expected RunOk, got {other:?}"),
            },
            _ => {}
        }
    }
    false
}

/// New-file hardening: an external write target that does not exist yet
/// must still reach the path-bounds approval flow. The shared normalizer
/// resolves the missing file through its existing parent, so the gate asks;
/// the routed consent then widens the real fence and the sandbox resolves
/// the new file for writing. The full tool chain over a reverse request is
/// the sibling protected-marker test.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_new_file_consent_writes() {
    let root = env::temp_dir().join(format!("newfile-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(&root).expect("mkdir root");
    let repo = root.join("repo");
    let outside = root.join("config");
    fs::create_dir_all(&repo).expect("mkdir repo");
    fs::create_dir_all(&outside).expect("mkdir outside");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let bounds = Arc::new(ContainmentAdapter(session.clone()));
    let gate: Arc<dyn ModeGate> = Arc::new(
        DefaultModeGate::new()
            .with_store(store.clone())
            .with_containment(bounds),
    );
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let server = Server::new(
        chain_runner(provider, ToolRegistry::new(), Some(session.clone())),
        SessionId::new(),
        gate.clone(),
    )
    .with_session(session.clone());

    // The not-yet-existing external file must ask at the pre-check.
    let target = outside.join("settings.json");
    let input = json!({"path": target.to_string_lossy()});
    let req = ToolRequest {
        tool_name: "write",
        input: Some(&input),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    match gate.decide(&req) {
        Decision::Ask(reason) => assert_eq!(reason.validator, "path-bounds"),
        other => panic!("outside write to a new file must ask, got {other:?}"),
    }

    // The routed consent widens the real fence and the new file is writable.
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds",
        detail: "path outside the workspace".into(),
        containment_note: None,
    };
    server
        .route_consent("write", &input, "once", Some(&reason))
        .expect("install write capability");
    session
        .write_file(&target.to_string_lossy(), b"{\"model\":{}}".to_vec())
        .await
        .expect("the approved new file is writable through the fence");
    assert_eq!(fs::read_to_string(&target).unwrap(), "{\"model\":{}}");

    let other = root.join("other");
    fs::create_dir_all(&other).expect("mkdir other");
    let deep = other.join("new-dir/sub/deep.json");
    let deep_input = json!({"path": deep.to_string_lossy()});
    let error = server
        .route_consent("write", &deep_input, "once", Some(&reason))
        .expect_err("a missing parent cannot receive a directory capability");
    assert!(matches!(error, SandboxError::NotFound(_)));
    assert!(
        !other.join("new-dir").exists(),
        "permission handling must not create tool target directories"
    );

    fs::remove_dir_all(&root).ok();
}

/// The reported settings-edit failure: an Edit of an existing external
/// settings file under a protected marker. The safety stage asks first, so
/// the approval's reason is protected-path and the path-bounds stage never
/// runs. Answering yes must still widen the fence through the same
/// directory grant the path-bounds flow uses, so the resumed edit writes
/// the file; a consent that only installs a rule leaves the fence closed
/// and the edit dies in the sandbox after the user already approved.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_protected_edit_approved_writes() {
    let root = env::temp_dir().join(format!("edit-chain-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(&root).expect("mkdir root");
    let repo = root.join("repo");
    let config = root.join("config").join(".houyicoder");
    fs::create_dir_all(&repo).expect("mkdir repo");
    fs::create_dir_all(&config).expect("mkdir config");
    let target = config.join("settings.json");
    fs::write(&target, b"{\"model\": \"old\"}").expect("seed settings");

    let sandbox: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let bounds = Arc::new(ContainmentAdapter(sandbox.clone()));
    let gate: Arc<dyn ModeGate> = Arc::new(
        DefaultModeGate::new()
            .with_store(store.clone())
            .with_containment(bounds.clone()),
    );
    let runner = guarded_edit_runner(sandbox.clone(), gate.clone(), &target);

    let (client_tx, server_rx) = mpsc::channel::<String>(16);
    let (server_tx, client_rx) = mpsc::channel::<String>(16);
    let server_io = FrameCarrier::new(server_tx, server_rx);
    let mut client = Client::new(Box::new(InProcTransport::from_halves(client_tx, client_rx)));
    let session = SessionId::new();
    let server = Server::new(runner, session, gate).with_session(sandbox.clone());
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    client.connect().await.expect("handshake");
    client
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: WireSessionId(session.to_string()),
                content: vec![ContentBlock::Text { text: "go".into() }],
                disabled_skills: Default::default(),
            },
        )
        .await
        .expect("send message");

    assert!(
        approve_once_and_finish(&mut client, RequestId(1), &config).await,
        "the resumed run produced a final outcome"
    );
    // The permission path the user just approved must land on disk; a fence
    // that never received the grant refuses the write the user approved.
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "{\"model\": \"new\"}",
        "the approved edit must land on disk"
    );

    drop(client);
    drop(handle.await);
    fs::remove_dir_all(&root).ok();
}

/// A server rebuilt for a reattaching connection recovers the runner's
/// sandbox session, so an approved pending call can install its capability.
#[cfg(target_os = "macos")]
#[test]
fn test_resume_server_restores_sandbox() {
    let root = env::temp_dir().join(format!("resume-fence-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&root));
    let repo = root.join("repo");
    let config = root.join("config").join(".houyicoder");
    fs::create_dir_all(&repo).expect("mkdir repo");
    fs::create_dir_all(&config).expect("mkdir config");
    let target = config.join("settings.json");
    fs::write(&target, b"old").expect("seed settings");

    let sandbox: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let gate: Arc<dyn ModeGate> = Arc::new(
        DefaultModeGate::new().with_containment(Arc::new(ContainmentAdapter(sandbox.clone()))),
    );
    let runner = guarded_edit_runner(sandbox.clone(), gate.clone(), &target);
    let session = SessionId::new();
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    let server = Server::new_for_resume(
        runner,
        session,
        EventSequencer::new(),
        gate,
        host,
        Arc::new(tokio::sync::Notify::new()),
    );
    let input = json!({"path": target.to_string_lossy()});
    let reason = AskReason {
        source: AskSource::SystemSafety,
        validator: "protected_path",
        detail: "protected path".into(),
        containment_note: None,
    };
    server
        .route_consent("edit", &input, "once", Some(&reason))
        .expect("reattached server installs the directory capability");
    sandbox
        .resolve_write(&target.to_string_lossy())
        .expect("the recovered fence admits the approved target");

    fs::remove_dir_all(&root).ok();
}
