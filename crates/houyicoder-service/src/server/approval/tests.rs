//! Unit contracts for permission requests, the carried ask cause, scoped
//! consent routing, and approval-card detail.

use super::*;
use crate::composition::SkillRegistryImpl;
use crate::protocol_adapter::{CardFence, FENCED};
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::{
    BoundaryAccess, BoundaryGrant, Containment, Coverage, FenceCapability, SandboxSession,
    SideEffect,
};
use houyicoder_api::skill::{SkillRegistry, SkillScriptRef};
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError, SessionId};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{ApprovalRequest, Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{
    Decision, DefaultModeGate, Effect, FileRuleStore, ModeGate, RuleStore, Scope, ToolRequest,
};
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{
    ClientResponseEnvelope, RequestEnvelope, RequestId, ServerRequestPayload,
};
use houyicoder_protocol::framing;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::{self, ApprovalDecision, ApprovalScope};
use houyicoder_protocol::frontend::trust::TrustAccept;
use houyicoder_provider::FakeProvider;
// Only the macOS-gated consent tests build a real platform session; the
// cross-platform tests drive the gate through a stub fence or read it
// directly, so on other platforms this import would be unused.
#[cfg(target_os = "macos")]
use houyicoder_sandbox::PlatformSession;
use houyicoder_session::SessionStore;
use serde_json::json;
use std::{
    env, fs,
    path::Path,
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// build_approval_request carries the structured Ask reason onto the request
/// form, and drops it to None when the ask names no cause, so the card
/// renders a generic prompt.
#[test]
fn test_build_request_carries_reason() {
    let req = ApprovalRequest::new("call-1".into(), "bash".into(), json!({"cmd": "rm"}), None);
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "destructive_command".to_string(),
        detail: "rm needs confirmation".into(),
        containment_note: None,
    };
    let request = build_approval_request(&req, Some(&reason), None, FENCED, &[]);
    assert_eq!(request.call_id, "call-1");
    assert_eq!(request.tool_name, "bash");
    let carried = request.reason.as_ref().expect("reason carried");
    assert_eq!(carried.detail, "rm needs confirmation");
    assert_eq!(carried.validator, "destructive_command");
    // The request form round-trips so the frontend reads the same reason.
    let json = serde_json::to_string(&request).unwrap();
    let back: run::ApprovalRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(back.reason.unwrap().detail, "rm needs confirmation");

    // None reason: the card falls back to a generic prompt.
    let no_reason = build_approval_request(&req, None, None, FENCED, &[]);
    assert!(no_reason.reason.is_none());
    let json = serde_json::to_string(&no_reason).unwrap();
    assert!(
        !json.contains("\"reason\""),
        "a None reason is skipped in the request: {json}"
    );
}

/// The card explains the call with the cause the gate reported when it judged
/// the call, even for a call the gate would not stop now: the verdict travels
/// on the request instead of being taken a second time when the card is
/// built.
#[tokio::test]
async fn test_card_carries_gate_reason() {
    let mut server = ask_wait_server();
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let reason = AskReason {
        source: AskSource::SystemSafety,
        validator: "path-bounds".to_string(),
        detail: "the gate named this path".to_string(),
        containment_note: None,
    };
    // A harmless command the gate allows: only a carried verdict can explain
    // why the card is up at all.
    let approval = ApprovalRequest::new(
        "c1".into(),
        "bash".into(),
        json!({"command": "echo hi"}),
        Some(reason),
    );
    let feeder = tokio::spawn(async move {
        for _ in 0..16 {
            let line = client_rx.next().await.expect("server frame");
            let Ok(ServerFrame::Request(req)) = serde_json::from_str::<ServerFrame>(&line) else {
                continue;
            };
            let ServerRequestPayload::Permission(card) = req.payload else {
                panic!("expected a permission ask");
            };
            let resp = ClientFrame::Response(ClientResponseEnvelope::new(
                req.req_id,
                ClientResponsePayload::Permission(ApprovalDecision {
                    call_id: "c1".into(),
                    approved: false,
                    updated_input: None,
                    scope: ApprovalScope::Once,
                }),
            ));
            client_tx
                .send(encode_line(&resp))
                .await
                .expect("send response");
            return card;
        }
        panic!("no permission ask reached the client");
    });
    let decision = server
        .handle_approval(&mut io, &approval, None)
        .await
        .expect("handle_approval completes");
    assert!(!decision.approved, "the verdict reaches the engine");
    let card = feeder.await.expect("feeder done");
    let carried = card.reason.expect("the card carries the gate's cause");
    assert_eq!(carried.detail, "the gate named this path");
    assert_eq!(carried.validator, "path-bounds");
}

/// The gate's protected-path ask for a Bash command that runs a script from a
/// discovered skill's directory arrives with a generic detail;
/// augment_skill_script_reason replaces it with the script's path so the
/// approval card shows what would execute.
#[test]
fn test_augment_skill_script() {
    let tmp = env::temp_dir().join(format!("recon-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&tmp));
    let skill_dir = tmp.join(".houyicoder").join("skills").join("deploy");
    fs::create_dir_all(skill_dir.join("scripts")).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: deploy skill\n---\nbody\n",
    )
    .unwrap();
    let reg: Arc<dyn SkillRegistry> =
        Arc::new(SkillRegistryImpl::discover_with_home(Some(&tmp), None));
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        sess_store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .with_skill_registry(reg);
    let server = Server::new(
        Arc::new(runner),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    );
    let cmd = {
        let canon = dunce::canonicalize(&skill_dir).unwrap();
        format!("python {}/scripts/deploy.py", canon.to_string_lossy())
    };
    let input = json!({"command": cmd});
    let judged = ToolRequest {
        tool_name: "bash",
        input: Some(&input),
        is_destructive: false,
        is_read_only: false,
        native_requires_approval: true,
    };
    let Decision::Ask(engine_reason) = server.gate.decide(&judged) else {
        panic!("gate asked on the skill-script path");
    };
    let mut reason = AskReason::from(&engine_reason);
    // Before the augment the detail is the generic protected-path
    // sentence; the augment replaces it with the script path.
    assert!(
        !reason.detail.contains("deploy/scripts/deploy.py"),
        "pre-augment detail is generic: {}",
        reason.detail
    );
    server.augment_skill_script_reason(&mut reason, "bash", &input);
    assert!(
        reason.detail.contains("deploy/scripts/deploy.py"),
        "post-augment detail names the script: {}",
        reason.detail
    );
    fs::remove_dir_all(&tmp).ok();
}

/// format_skill_script_detail names the first script and counts the rest
/// when a command runs more than one skill script, so the card stays one
/// line instead of listing every script.
#[test]
fn test_format_multi_script_detail() {
    let scripts = vec![
        SkillScriptRef {
            skill_name: "deploy".into(),
            script_rel_path: "scripts/a.py".into(),
        },
        SkillScriptRef {
            skill_name: "deploy".into(),
            script_rel_path: "scripts/b.py".into(),
        },
    ];
    let detail = format_skill_script_detail(&scripts);
    assert!(detail.contains("2 skill scripts"), "count: {detail}");
    assert!(
        detail.contains("deploy/scripts/a.py"),
        "first named: {detail}"
    );
}

/// augment_skill_script_reason leaves the detail untouched for a non-shell
/// tool: the detection is for bash commands, so an edit or write ask keeps
/// its original reason. The same command on a bash ask does augment.
#[test]
fn test_augment_skips_non_shell() {
    let tmp = env::temp_dir().join(format!("aug-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&tmp));
    let skill_dir = tmp.join(".houyicoder").join("skills").join("deploy");
    fs::create_dir_all(skill_dir.join("scripts")).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: deploy skill\n---\nbody\n",
    )
    .unwrap();
    let reg: Arc<dyn SkillRegistry> =
        Arc::new(SkillRegistryImpl::discover_with_home(Some(&tmp), None));
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        sess_store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .with_skill_registry(reg);
    let server = Server::new(
        Arc::new(runner),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    );
    let canon = dunce::canonicalize(&skill_dir).unwrap();
    let input = json!({"command": format!("python {}/scripts/deploy.py", canon.to_string_lossy())});

    // A non-shell tool: the detail stays as the gate wrote it.
    let mut reason = AskReason {
        source: AskSource::SystemSafety,
        validator: "protected_path".to_string(),
        detail: "original".into(),
        containment_note: None,
    };
    server.augment_skill_script_reason(&mut reason, "edit", &input);
    assert_eq!(
        reason.detail, "original",
        "non-shell tool: detail unchanged"
    );

    // The same command on a bash ask: the detail names the script.
    server.augment_skill_script_reason(&mut reason, "bash", &input);
    assert!(
        reason.detail.contains("deploy/scripts/deploy.py"),
        "bash: detail augmented: {}",
        reason.detail
    );
    fs::remove_dir_all(&tmp).ok();
}

/// Answering always must reach both layers: the fence makes this run work,
/// the store makes it survive a restart. Fence only and the grant is
/// forgotten next launch; store only and the run that just asked still
/// refuses the path. macOS-only: widening a live fence is Seatbelt-only.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_consent_reaches_both_layers() {
    let root = env::temp_dir().join(format!("consent-dir-{}", process::id()));
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(&root).expect("mkdir root");
    let outside = root.join("outside");
    fs::create_dir_all(&outside).expect("mkdir outside");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let gate = Arc::new(DefaultModeGate::new().with_store(store.clone()));
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        sess_store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let server =
        Server::new(Arc::new(runner), SessionId::new(), gate).with_session(session.clone());
    let input = json!({"path": outside.to_string_lossy(), "pattern": "x"});
    server
        .apply_consent_directory("grep", &input, ApprovalScope::Workspace)
        .expect("grant read directory");
    let coutside = dunce::canonicalize(&outside).unwrap();
    let dirs = session.working_dirs();
    assert!(
        dirs.iter().any(|d| Path::new(d) == coutside),
        "fence (additional_dirs) has the granted dir: {dirs:?}"
    );
    let stored = store.load_read_directories();
    assert!(
        stored.iter().any(|d| d == &coutside),
        "durable store persisted the granted dir: {stored:?}"
    );
    assert!(
        session
            .write_file(
                &outside.join("blocked.txt").to_string_lossy(),
                b"blocked".to_vec(),
            )
            .await
            .is_err(),
        "a grep approval must not authorize host writes"
    );

    let config_dir = root.join("config");
    fs::create_dir_all(&config_dir).expect("mkdir config");
    let file_path = config_dir.join("settings.json");
    fs::write(&file_path, b"{}").expect("write file");
    let input = json!({"path": file_path.to_string_lossy()});
    server
        .apply_consent_directory("edit", &input, ApprovalScope::Once)
        .expect("grant write directory");
    let canonical_config = dunce::canonicalize(&config_dir).unwrap();
    assert!(
        session
            .working_dirs()
            .iter()
            .any(|dir| Path::new(dir) == canonical_config),
        "an approved file edit grants its parent directory for this session"
    );
    assert!(
        !store
            .load_directories()
            .iter()
            .any(|dir| dir == &canonical_config),
        "once grants the live fence without persisting the directory"
    );
    session
        .write_file(&file_path.to_string_lossy(), b"{\"model\":{}}".to_vec())
        .await
        .expect("the approved file path is writable through the host tool fence");
    assert_eq!(fs::read_to_string(&file_path).unwrap(), "{\"model\":{}}");

    fs::remove_dir_all(&root).ok();
}

/// The directory grant follows the approved target, not the ask reason: a
/// call whose resolved path sits outside the fence widens it whichever
/// validator asked, because an approval that cannot execute is not an
/// approval. The reason selects the rule path: a non-path-bounds ask
/// answered with scope always persists a tool rule, while a path-bounds
/// answer persists the directory instead. An inside target grants no
/// directory. macOS-only: widening a live fence is Seatbelt-only.
#[cfg(target_os = "macos")]
#[test]
fn test_consent_grants_by_location() {
    let root = env::temp_dir().join(format!("route-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(&root).expect("mkdir root");
    let outside = root.join("outside");
    let second = root.join("second");
    fs::create_dir_all(&outside).expect("mkdir outside");
    fs::create_dir_all(&second).expect("mkdir second");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let gate = Arc::new(DefaultModeGate::new().with_store(store.clone()));
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        sess_store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let server =
        Server::new(Arc::new(runner), SessionId::new(), gate).with_session(session.clone());

    // A path-bounds ask on an outside target, scope always: the read
    // directory reaches both layers and no tool rule is persisted.
    let input = json!({"path": outside.to_string_lossy(), "pattern": "x"});
    let path_bounds = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds".to_string(),
        detail: "path outside the workspace".into(),
        containment_note: None,
    };
    server
        .route_consent("grep", &input, ApprovalScope::Workspace, Some(&path_bounds))
        .expect("route path-bounds consent");
    let coutside = dunce::canonicalize(&outside).unwrap();
    assert!(
        store.load_read_directories().iter().any(|d| d == &coutside),
        "a path-bounds answer grants the read directory"
    );
    assert!(
        store.load().iter().all(|r| r.action != "grep"),
        "a path-bounds answer persists the directory, not a tool rule"
    );

    // A different validator asked, but the approved target is still outside
    // the fence, so the grant follows the target. Scope once: the live fence
    // widens, the store does not, and no rule is persisted.
    let user_ask = AskReason {
        source: AskSource::UserRule,
        validator: "some-rule".to_string(),
        detail: "user rule fired".into(),
        containment_note: None,
    };
    let second_input = json!({"path": second.to_string_lossy(), "pattern": "x"});
    server
        .route_consent("grep", &second_input, ApprovalScope::Once, Some(&user_ask))
        .expect("route one-time consent");
    let csecond = dunce::canonicalize(&second).unwrap();
    assert!(
        session
            .working_dirs()
            .iter()
            .any(|d| Path::new(d) == csecond),
        "an outside target widens the live fence whichever validator asked"
    );
    assert!(
        !store.load_read_directories().iter().any(|d| d == &csecond),
        "scope once grants the live fence without persisting"
    );
    assert!(
        store.load().iter().all(|r| r.action != "grep"),
        "scope once persists no rule"
    );

    // An inside target with a rule ask: nothing to widen, so no directory
    // grant; scope always persists the tool rule the answer chose.
    let inner = repo.join("inner.txt");
    fs::write(&inner, b"x").expect("write inner");
    let inner_input = json!({"path": inner.to_string_lossy(), "pattern": "x"});
    server
        .route_consent(
            "grep",
            &inner_input,
            ApprovalScope::Workspace,
            Some(&user_ask),
        )
        .expect("route inside consent");
    assert_eq!(
        store.load_read_directories(),
        vec![coutside],
        "an inside target grants no directory"
    );
    assert!(
        store.load().iter().any(|r| r.action == "grep"),
        "a non-path-bounds always answer persists the tool rule"
    );

    fs::remove_dir_all(&root).ok();
}

/// A session-lifetime answer keeps the rule in the running gate and writes
/// nothing to a store, so the next start inherits no preference this session
/// did not outlive. The choice decides, not the grant plan: this ask could
/// have kept a workspace lifetime, and a session choice narrows it.
#[test]
fn test_session_rule_not_stored() {
    let root = env::temp_dir().join(format!("session-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(&root).expect("mkdir root");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let gate = Arc::new(DefaultModeGate::new().with_store(store.clone()));
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("test")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let server = Server::new(Arc::new(runner), SessionId::new(), gate.clone());

    let inner = repo.join("inner.txt");
    fs::write(&inner, b"x").expect("write inner");
    let input = json!({"path": inner.to_string_lossy(), "pattern": "x"});
    let reason = AskReason {
        source: AskSource::UserRule,
        validator: "some-rule".to_string(),
        detail: "user rule fired".into(),
        containment_note: None,
    };
    server
        .route_consent("grep", &input, ApprovalScope::Session, Some(&reason))
        .expect("route session consent");
    assert!(
        gate.rules()
            .iter()
            .any(|rule| rule.action == "grep" && rule.scope == Scope::Session),
        "the session answer takes effect in the running gate"
    );
    assert!(
        store.load().is_empty(),
        "a session-lifetime rule reaches no store"
    );

    fs::remove_dir_all(&root).ok();
}

/// A session-lifetime directory answer widens this session's fence and
/// writes nothing to the store, so a later start neither re-attaches the
/// directory nor remembers a decision the session ended with. Widening a live
/// fence is Seatbelt-only.
#[cfg(target_os = "macos")]
#[test]
fn test_session_grant_not_stored() {
    let root = env::temp_dir().join(format!("session-dir-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&root));
    let outside = root.join("outside");
    let repo = root.join("repo");
    fs::create_dir_all(&outside).expect("mkdir outside");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let gate = Arc::new(DefaultModeGate::new().with_store(store.clone()));
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("test")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let server =
        Server::new(Arc::new(runner), SessionId::new(), gate).with_session(session.clone());
    let target = outside.join("report.txt");
    fs::write(&target, b"x").expect("write target");
    let input = json!({"path": target.to_string_lossy(), "pattern": "x"});
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds".to_string(),
        detail: "path outside the workspace".into(),
        containment_note: None,
    };
    server
        .route_consent("grep", &input, ApprovalScope::Session, Some(&reason))
        .expect("route session directory consent");
    let coutside = dunce::canonicalize(&outside).unwrap();
    assert!(
        session
            .working_dirs()
            .iter()
            .any(|dir| Path::new(dir) == coutside),
        "the session answer widens this session's fence"
    );
    assert!(
        store.load_read_directories().is_empty(),
        "a session-lifetime directory grant reaches no store"
    );
    assert!(
        store.load().iter().all(|rule| rule.action != "grep"),
        "a path-bounds answer persists no tool rule at any lifetime"
    );

    fs::remove_dir_all(&root).ok();
}

/// A system-safety approval lets the frozen call through and persists
/// nothing, even when the client answers with the workspace lifetime: the ask
/// offers no remember option, so a guest cannot widen it into a saved grant
/// for every later protected edit.
#[cfg(target_os = "macos")]
#[test]
fn test_safety_persists_nothing() {
    let root = env::temp_dir().join(format!("safety-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&root));
    let repo = root.join("repo");
    let protected = root.join("protected").join(".houyicoder");
    fs::create_dir_all(&repo).expect("mkdir repo");
    fs::create_dir_all(&protected).expect("mkdir protected");
    let settings = protected.join("settings.json");
    fs::write(&settings, b"{}").expect("write settings");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let gate = Arc::new(DefaultModeGate::new().with_store(store.clone()));
    let sandbox: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("test")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let server = Server::new(Arc::new(runner), SessionId::new(), gate).with_session(sandbox);
    let input = json!({"path": settings.to_string_lossy()});
    let reason = AskReason {
        source: AskSource::SystemSafety,
        validator: "protected_path".to_string(),
        detail: "protected path".into(),
        containment_note: None,
    };

    server
        .route_consent("edit", &input, ApprovalScope::Workspace, Some(&reason))
        .expect("route protected consent");
    let canonical = dunce::canonicalize(&protected).unwrap();
    assert!(
        server
            .sandbox_session
            .as_ref()
            .expect("session")
            .working_dirs()
            .iter()
            .any(|dir| Path::new(dir) == canonical),
        "the frozen call can reach the directory it was approved for"
    );
    assert!(
        store.load_directories().is_empty(),
        "a system-safety ask keeps no durable directory grant"
    );
    assert!(store.load().iter().all(|rule| rule.action != "edit"));

    fs::remove_dir_all(&root).ok();
}

/// A fence whose runtime directory grants always succeed and are remembered,
/// so the platform-independent consent rule logic can be exercised on every
/// platform. A real platform session supports runtime grants only on macOS;
/// the Linux and Windows fences are fixed at construction and report
/// Unsupported, which would make this rule test fail elsewhere even though
/// the rule logic it targets is not platform-specific. Remembering the grants
/// lets a later call see an earlier one's directory as already in bounds, so
/// the location-check skip the test documents holds on every platform.
struct GrantFence {
    root: Arc<Path>,
    granted: Mutex<Vec<String>>,
}

impl SandboxSession for GrantFence {
    fn workspace_root(&self) -> Arc<Path> {
        self.root.clone()
    }
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn add_reading_dir(&self, path: &str) -> Result<(), SandboxError> {
        self.granted
            .lock()
            .expect("grant lock")
            .push(path.to_string());
        Ok(())
    }
    fn working_dirs(&self) -> Vec<String> {
        self.granted.lock().expect("grant lock").clone()
    }
}

/// A None reason, meaning the ask named no cause, must never reach the rule
/// path: its non-bash terminal is a contentless tool-level allow that would
/// shadow every later ask. The second call targets a path already inside the
/// fence after the first grant, which is where a cause is easiest to lose:
/// the location check skips the granted directory, so nothing broad may be
/// installed on the strength of a missing reason.
#[test]
fn test_none_reason_no_rule() {
    let root = env::temp_dir().join(format!("none-{}-{}", process::id(), line!()));
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(&root).expect("mkdir root");
    let outside = root.join("outside");
    let nested = outside.join("sub");
    fs::create_dir_all(&nested).expect("mkdir nested");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let gate = Arc::new(DefaultModeGate::new().with_store(store.clone()));
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> = Arc::new(GrantFence {
        root: Arc::from(repo),
        granted: Mutex::new(Vec::new()),
    });
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        sess_store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let server =
        Server::new(Arc::new(runner), SessionId::new(), gate).with_session(session.clone());

    // First approval in the batch: path-bounds, scope always grants the
    // outside directory to the fence and store.
    let outside_input = json!({"path": outside.to_string_lossy(), "pattern": "x"});
    let path_bounds = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds".to_string(),
        detail: "path outside the workspace".into(),
        containment_note: None,
    };
    server
        .route_consent(
            "grep",
            &outside_input,
            ApprovalScope::Workspace,
            Some(&path_bounds),
        )
        .expect("route outside consent");

    // Second approval with no cause carried: the nested path is already inside
    // the granted directory, so the location check skips it and the reason is
    // None.
    let nested_input = json!({"path": nested.to_string_lossy(), "pattern": "y"});
    server
        .route_consent("grep", &nested_input, ApprovalScope::Workspace, None)
        .expect("route nested consent");

    let blanket_grep_allow = store
        .load()
        .iter()
        .any(|r| r.action == "grep" && r.content.is_none() && r.effect == Effect::Allow);
    assert!(
        !blanket_grep_allow,
        "a None-reason grep must not install a contentless blanket allow rule: {:?}",
        store.load()
    );

    // The fence remembers the first grant, so the second call's nested path is
    // already in bounds and the location check skips it: only the outside
    // grant is recorded. The recording fence is what makes this documented
    // skip hold on every platform, not only where a real session remembers
    // grants.
    let recorded = session.working_dirs();
    assert_eq!(
        recorded.len(),
        1,
        "only the outside grant is recorded; the nested call must be skipped: {recorded:?}"
    );

    fs::remove_dir_all(&root).ok();
}

fn ask_wait_server() -> Server {
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        sess_store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    Server::new(
        Arc::new(runner),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    )
}

fn encode_line(msg: &impl serde::Serialize) -> String {
    let mut f = framing::encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    f
}

/// A non-matching frame mid-ask, here a Status Request, is dropped, not
/// fatal; the ask-wait keeps waiting and pairs the permission response
/// that follows.
#[tokio::test]
async fn test_handle_drops_non_matching() {
    let mut server = ask_wait_server();
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let approval = ApprovalRequest::new(
        "c1".into(),
        "bash".into(),
        json!({"command": "echo hi"}),
        None,
    );
    let feeder = tokio::spawn(async move {
        // Read the Permission ask to learn its req_id.
        let mut ask_id = None;
        for _ in 0..16 {
            let line = client_rx.next().await.expect("server frame");
            if let Ok(ServerFrame::Request(req)) = serde_json::from_str(&line) {
                ask_id = Some(req.req_id);
                break;
            }
        }
        let ask_id = ask_id.expect("permission ask sent");
        // A non-matching Status Request mid-ask.
        let status = ClientFrame::Request(RequestEnvelope::new(
            RequestId(999),
            FrontendRequest::Status,
        ));
        client_tx
            .send(encode_line(&status))
            .await
            .expect("send status");
        // The matching permission response.
        let resp = ClientFrame::Response(ClientResponseEnvelope::new(
            ask_id,
            ClientResponsePayload::Permission(ApprovalDecision {
                call_id: "c1".into(),
                approved: true,
                updated_input: None,
                scope: ApprovalScope::Once,
            }),
        ));
        client_tx
            .send(encode_line(&resp))
            .await
            .expect("send response");
    });
    let decision = server
        .handle_approval(&mut io, &approval, None)
        .await
        .expect("handle_approval did not fatal on the non-matching frame");
    feeder.await.expect("feeder done");
    assert!(
        decision.approved,
        "the matching response paired after the non-matching frame was dropped"
    );
}

/// A session/cancel mid-ask aborts the run and returns a deny so the serve
/// loop resumes the cancelled run instead of hanging on a response the
/// client will not send.
#[tokio::test]
async fn test_handle_cancel_returns_deny() {
    let mut server = ask_wait_server();
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let approval = ApprovalRequest::new(
        "c1".into(),
        "bash".into(),
        json!({"command": "echo hi"}),
        None,
    );
    let feeder = tokio::spawn(async move {
        // Read the Permission ask (a ServerFrame::Request), then send
        // session/cancel.
        for _ in 0..16 {
            let line = client_rx.next().await.expect("server frame");
            if serde_json::from_str::<ServerFrame>(&line).is_ok() {
                break;
            }
        }
        let cancel = AcpNotification::new("session/cancel", json!({}));
        client_tx
            .send(encode_line(&cancel))
            .await
            .expect("send cancel");
    });
    let decision = server
        .handle_approval(&mut io, &approval, None)
        .await
        .expect("handle_approval returned on cancel");
    feeder.await.expect("feeder done");
    assert!(
        !decision.approved,
        "cancel mid-ask returns a deny, not a hang"
    );
}

/// A reverse response with the wrong payload shape fails closed: the ask
/// expects a permission decision, and a trust payload is a protocol
/// violation rather than a frame to drop.
#[tokio::test]
async fn test_handle_rejects_wrong_payload() {
    let mut server = ask_wait_server();
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let approval = ApprovalRequest::new(
        "c1".into(),
        "bash".into(),
        json!({"command": "echo hi"}),
        None,
    );
    let feeder = tokio::spawn(async move {
        let mut ask_id = None;
        for _ in 0..16 {
            let line = client_rx.next().await.expect("server frame");
            if let Ok(ServerFrame::Request(req)) = serde_json::from_str(&line) {
                ask_id = Some(req.req_id);
                break;
            }
        }
        let ask_id = ask_id.expect("permission ask sent");
        let resp = ClientFrame::Response(ClientResponseEnvelope::new(
            ask_id,
            ClientResponsePayload::TrustAccept(TrustAccept { accepted: true }),
        ));
        client_tx
            .send(encode_line(&resp))
            .await
            .expect("send the wrong payload");
    });
    let err = server
        .handle_approval(&mut io, &approval, None)
        .await
        .expect_err("a trust payload cannot answer a permission ask");
    feeder.await.expect("feeder done");
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert_eq!(err.message, "expected a permission reverse response");
}

/// A fence whose directory grants may or may not move its capability
/// revision, so one install path can be driven both ways.
struct RevisionFence {
    root: Arc<Path>,
    granted: Mutex<Vec<String>>,
    revision: AtomicU64,
    advance_on_grant: bool,
}

impl Containment for RevisionFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![self.root.to_path_buf()],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn capability_revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }
}

impl SandboxSession for RevisionFence {
    fn workspace_root(&self) -> Arc<Path> {
        self.root.clone()
    }
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn add_reading_dir(&self, path: &str) -> Result<(), SandboxError> {
        self.granted
            .lock()
            .expect("grant lock")
            .push(path.to_string());
        if self.advance_on_grant {
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn remove_working_dir(&self, path: &str) {
        self.granted
            .lock()
            .expect("grant lock")
            .retain(|granted| granted != path);
    }
    fn working_dirs(&self) -> Vec<String> {
        self.granted.lock().expect("grant lock").clone()
    }
    fn as_containment(&self) -> Option<&dyn Containment> {
        Some(self)
    }
}

/// A grant the fence accepts without moving its own revision did not take
/// effect, so the install fails and the runtime change is rolled back rather
/// than reported as saved. The control with a fence that widens shows the
/// check follows the revision and not the count of grants.
#[test]
fn test_grant_requires_fence_widening() {
    let root = env::temp_dir().join(format!("revision-{}", process::id()));
    drop(fs::remove_dir_all(&root));
    let outside = root.join("outside");
    fs::create_dir_all(&outside).expect("mkdir outside");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let input = json!({"path": outside.to_string_lossy(), "pattern": "x"});

    let inert = Arc::new(RevisionFence {
        root: Arc::from(repo.as_path()),
        granted: Mutex::new(Vec::new()),
        revision: AtomicU64::new(3),
        advance_on_grant: false,
    });
    let server = server_over_fence(inert.clone());
    let err = server
        .apply_consent_directory("grep", &input, ApprovalScope::Once)
        .expect_err("an install the fence did not apply must fail");
    assert!(matches!(err, SandboxError::Unsupported(_)));
    assert!(
        inert.working_dirs().is_empty(),
        "the failed install is rolled back: {:?}",
        inert.working_dirs()
    );

    let growing = Arc::new(RevisionFence {
        root: Arc::from(repo.as_path()),
        granted: Mutex::new(Vec::new()),
        revision: AtomicU64::new(3),
        advance_on_grant: true,
    });
    let server = server_over_fence(growing.clone());
    server
        .apply_consent_directory("grep", &input, ApprovalScope::Once)
        .expect("a widening fence keeps the grant");
    assert_eq!(growing.working_dirs().len(), 1);

    // A grant the session already holds changes nothing, so a fence that
    // does not move its revision must not fail the repeat install.
    let again = server.apply_consent_directory("grep", &input, ApprovalScope::Once);
    assert!(again.is_ok(), "a repeated grant is not a failed install");
    drop(fs::remove_dir_all(&root));
}

fn server_over_fence(session: Arc<RevisionFence>) -> Server {
    let gate = Arc::new(DefaultModeGate::new());
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        sess_store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    Server::new(Arc::new(runner), SessionId::new(), gate).with_session(session)
}

/// A fence that reports one writes axis and one egress verdict, so the card
/// facts can be read off without depending on what the host platform
/// enforces.
struct AxisFence {
    writes: bool,
    egress: Option<&'static str>,
}

impl Containment for AxisFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: Vec::new(),
        }
    }
    fn capability(&self) -> FenceCapability {
        FenceCapability {
            writes: self.writes,
            reads: false,
            network: false,
        }
    }
    fn would_block(&self, effect: SideEffect) -> Option<String> {
        match effect {
            SideEffect::Network => self.egress.map(str::to_string),
            _ => None,
        }
    }
}

/// A session that keeps read and write grants apart and canonicalizes on
/// the way in, the way a real runtime fence stores them.
struct SplitGrantFence {
    root: Arc<Path>,
    reads: Mutex<Vec<String>>,
    writes: Mutex<Vec<String>>,
}

impl SplitGrantFence {
    fn canonical(path: &str) -> Option<String> {
        dunce::canonicalize(path)
            .ok()
            .map(|canonical| canonical.to_string_lossy().into_owned())
    }
}

impl Containment for SplitGrantFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: Vec::new(),
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
}

impl SandboxSession for SplitGrantFence {
    fn workspace_root(&self) -> Arc<Path> {
        self.root.clone()
    }
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn add_reading_dir(&self, path: &str) -> Result<(), SandboxError> {
        let Some(canonical) = Self::canonical(path) else {
            return Err(SandboxError::NotFound(path.to_string()));
        };
        let writes = self.writes.lock().expect("write lock");
        if writes.contains(&canonical) {
            return Ok(());
        }
        drop(writes);
        let mut reads = self.reads.lock().expect("read lock");
        if !reads.contains(&canonical) {
            reads.push(canonical);
        }
        Ok(())
    }
    fn add_working_dir(&self, path: &str) -> Result<(), SandboxError> {
        let Some(canonical) = Self::canonical(path) else {
            return Err(SandboxError::NotFound(path.to_string()));
        };
        self.reads
            .lock()
            .expect("read lock")
            .retain(|held| held != &canonical);
        let mut writes = self.writes.lock().expect("write lock");
        if !writes.contains(&canonical) {
            writes.push(canonical);
        }
        Ok(())
    }
    fn remove_working_dir(&self, path: &str) {
        let Some(canonical) = Self::canonical(path) else {
            return;
        };
        self.reads
            .lock()
            .expect("read lock")
            .retain(|held| held != &canonical);
        self.writes
            .lock()
            .expect("write lock")
            .retain(|held| held != &canonical);
    }
    fn reading_dirs(&self) -> Vec<String> {
        self.reads.lock().expect("read lock").clone()
    }
    fn working_dirs(&self) -> Vec<String> {
        let mut all = self.reading_dirs();
        all.extend(self.writing_dirs());
        all
    }
    fn writing_dirs(&self) -> Vec<String> {
        self.writes.lock().expect("write lock").clone()
    }
    fn as_containment(&self) -> Option<&dyn Containment> {
        Some(self)
    }
}

/// A failed install must undo what it added and nothing else. The directory
/// below held a read grant before the install asked for read-write, so the
/// rollback restores the read grant; removing the directory outright would
/// revoke an authorization the user made earlier.
#[test]
fn test_rollback_restores_read_grant() {
    let root = env::temp_dir().join(format!("rollback-{}", process::id()));
    drop(fs::remove_dir_all(&root));
    let held = root.join("held-read");
    fs::create_dir_all(&held).expect("mkdir held");
    let missing = root.join("missing");
    let session = Arc::new(SplitGrantFence {
        root: Arc::from(root.as_path()),
        reads: Mutex::new(Vec::new()),
        writes: Mutex::new(Vec::new()),
    });
    let held_text = held.to_string_lossy().into_owned();
    session
        .add_reading_dir(&held_text)
        .expect("seed read grant");
    let server = server_over_gate(Arc::new(DefaultModeGate::new()))
        .with_session(session.clone() as Arc<dyn SandboxSession>);

    let grants = [
        BoundaryGrant {
            directory: held.clone(),
            access: BoundaryAccess::ReadWrite,
        },
        BoundaryGrant {
            directory: missing,
            access: BoundaryAccess::ReadOnly,
        },
    ];
    server
        .install_directory_grants(&grants, ApprovalScope::Once)
        .expect_err("a missing directory fails the install");

    let canonical = dunce::canonicalize(&held).expect("canonicalize held");
    let canonical = canonical.to_string_lossy().into_owned();
    assert!(
        session.reading_dirs().contains(&canonical),
        "the earlier read grant is restored: {:?}",
        session.reading_dirs()
    );
    assert!(
        !session.writing_dirs().contains(&canonical),
        "the upgrade is undone: {:?}",
        session.writing_dirs()
    );
    drop(fs::remove_dir_all(&root));
}

fn server_over_gate(gate: Arc<dyn ModeGate>) -> Server {
    let sess_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        sess_store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    Server::new(Arc::new(runner), SessionId::new(), gate)
}

/// Each card fact is taken from the axis it belongs to, so a fence that does
/// not confine writes cannot offer a remembered command and a fence that
/// always refuses egress cannot offer a remembered destination. Deriving
/// either fact from coverage alone, or swapping the two, reopens a card that
/// promises more than the kernel keeps.
#[test]
fn test_card_fence_reads_axes() {
    let withdrawn = server_over_gate(Arc::new(DefaultModeGate::new().with_containment(Arc::new(
        AxisFence {
            writes: false,
            egress: Some("the network policy denies egress"),
        },
    ))));
    let facts = withdrawn.card_fence();
    assert_eq!(
        facts,
        CardFence {
            writes_confined: false,
            egress_blocked: true,
        }
    );

    let confined = server_over_gate(Arc::new(DefaultModeGate::new().with_containment(Arc::new(
        AxisFence {
            writes: true,
            egress: None,
        },
    ))));
    let facts = confined.card_fence();
    assert_eq!(
        facts,
        CardFence {
            writes_confined: true,
            egress_blocked: false,
        }
    );
}

/// The chain tests build a real platform sandbox session, so the whole
/// submodule is platform-gated rather than each test inside it.
#[cfg(target_os = "macos")]
mod consent_chain;
