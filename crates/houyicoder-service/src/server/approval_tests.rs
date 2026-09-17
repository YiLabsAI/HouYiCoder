//! Unit contracts for permission requests, reason reconstruction, scoped
//! consent routing, and approval-card detail.

use super::*;
use crate::composition::SkillRegistryImpl;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::skill::{SkillRegistry, SkillScriptRef};
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{ApprovalRequest, Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{
    AskReason, AskSource, DefaultModeGate, Effect, FileRuleStore, RuleStore,
};
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{ClientResponseEnvelope, RequestEnvelope, RequestId};
use houyicoder_protocol::framing;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::{self, ApprovalDecision};
use houyicoder_protocol::frontend::trust::TrustAccept;
use houyicoder_provider::FakeProvider;
use houyicoder_sandbox::PlatformSession;
use houyicoder_session::SessionStore;
use serde_json::json;
use std::{env, fs, path::Path, process, sync::Arc};

/// build_approval_request carries the structured Ask reason the gate
/// produced onto the request form, and drops it to None when the
/// composition root could not reconstruct one, so the card renders a
/// generic prompt.
#[test]
fn test_build_request_carries_reason() {
    let req = ApprovalRequest::new("call-1".into(), "bash".into(), json!({"cmd": "rm"}));
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "destructive_command",
        detail: "rm needs confirmation".into(),
        containment_note: None,
    };
    let wired = build_approval_request(&req, Some(&reason), None);
    assert_eq!(wired.call_id, "call-1");
    assert_eq!(wired.tool_name, "bash");
    let carried = wired.reason.as_ref().expect("reason carried");
    assert_eq!(carried.detail, "rm needs confirmation");
    assert_eq!(carried.validator, "destructive_command");
    // The request form round-trips so the frontend reads the same reason.
    let json = serde_json::to_string(&wired).unwrap();
    let back: run::ApprovalRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(back.reason.unwrap().detail, "rm needs confirmation");

    // None reason: the card falls back to a generic prompt.
    let no_reason = build_approval_request(&req, None, None);
    assert!(no_reason.reason.is_none());
    let json = serde_json::to_string(&no_reason).unwrap();
    assert!(
        !json.contains("\"reason\""),
        "a None reason is skipped in the request: {json}"
    );
}

/// reconstruct_reason returns the gate's protected-path ask for a Bash
/// command that runs a script from a discovered skill's directory, and
/// augment_skill_script_reason then replaces the generic detail with the
/// script's path so the approval card shows what would execute. The two
/// steps mirror handle_approval's order.
#[test]
fn test_reconstruct_skill_script() {
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
    let mut reason = server
        .reconstruct_reason("bash", &input)
        .expect("gate asked on the skill-script path");
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
        validator: "protected_path",
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
        .apply_consent_directory("grep", &input, "always")
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
        .apply_consent_directory("edit", &input, "once")
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
        validator: "path-bounds",
        detail: "path outside the workspace".into(),
        containment_note: None,
    };
    server
        .route_consent("grep", &input, "always", Some(&path_bounds))
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
        validator: "some-rule",
        detail: "user rule fired".into(),
        containment_note: None,
    };
    let second_input = json!({"path": second.to_string_lossy(), "pattern": "x"});
    server
        .route_consent("grep", &second_input, "once", Some(&user_ask))
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
        .route_consent("grep", &inner_input, "always", Some(&user_ask))
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

/// A system-safety approval grants the disclosed directory without turning
/// one protected edit into a blanket allow rule for every edit.
#[cfg(target_os = "macos")]
#[test]
fn test_safety_skips_rule() {
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
        validator: "protected_path",
        detail: "protected path".into(),
        containment_note: None,
    };

    server
        .route_consent("edit", &input, "always", Some(&reason))
        .expect("route protected consent");
    let canonical = dunce::canonicalize(&protected).unwrap();
    assert!(store.load_directories().contains(&canonical));
    assert!(store.load().iter().all(|rule| rule.action != "edit"));

    fs::remove_dir_all(&root).ok();
}

/// A None reason, meaning the re-decide could not reproduce why the gate
/// asked, must never reach the rule path: its non-bash terminal is a
/// contentless tool-level allow that would shadow every later ask. The
/// second call targets a path already inside the fence after the first
/// grant, so the location check skips the directory too and the None route
/// persists nothing.
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

    // First approval in the batch: path-bounds, scope always grants the
    // outside directory to the fence and store.
    let outside_input = json!({"path": outside.to_string_lossy(), "pattern": "x"});
    let path_bounds = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds",
        detail: "path outside the workspace".into(),
        containment_note: None,
    };
    server
        .route_consent("grep", &outside_input, "always", Some(&path_bounds))
        .expect("route outside consent");

    // Second approval: the path is now inside the granted directory, so the
    // re-decide returns Allow and the reason is None.
    let nested_input = json!({"path": nested.to_string_lossy(), "pattern": "y"});
    server
        .route_consent("grep", &nested_input, "always", None)
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
    let approval = ApprovalRequest::new("c1".into(), "bash".into(), json!({"command": "echo hi"}));
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
                scope: "once".to_string(),
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
    let approval = ApprovalRequest::new("c1".into(), "bash".into(), json!({"command": "echo hi"}));
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
    let approval = ApprovalRequest::new("c1".into(), "bash".into(), json!({"command": "echo hi"}));
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
