//! Tests for the mid-run permission reverse-request (handle_approval,
//! reconstruct_reason, route_consent, the skill-script augment). Split
//! from approval.rs so that file stays under the file-size gate.

use super::*;
use houyicoder_permission::{AskReason, AskSource};

/// build_approval_request carries the structured Ask reason the gate
/// produced onto the wire form, and drops it to None when the composition
/// root could not reconstruct one (the card then renders a generic prompt).
#[test]
fn test_build_request_carries_reason() {
    let req = houyicoder_core::agent::ApprovalRequest::new(
        "call-1".into(),
        "bash".into(),
        serde_json::json!({"cmd": "rm"}),
    );
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "destructive_command",
        detail: "rm needs confirmation".into(),
        containment_note: None,
    };
    let wired = build_approval_request(&req, Some(&reason), None);
    assert_eq!(wired.call_id, "call-1");
    assert_eq!(wired.tool_name, "bash");
    let wire_reason = wired.reason.as_ref().expect("reason carried onto the wire");
    assert_eq!(wire_reason.detail, "rm needs confirmation");
    assert_eq!(wire_reason.validator, "destructive_command");
    // The wire form round-trips so the frontend reads the same reason.
    let json = serde_json::to_string(&wired).unwrap();
    let back: houyicoder_protocol::frontend::run::ApprovalRequest =
        serde_json::from_str(&json).unwrap();
    assert_eq!(back.reason.unwrap().detail, "rm needs confirmation");

    // None reason: the card falls back to a generic prompt.
    let no_reason = build_approval_request(&req, None, None);
    assert!(no_reason.reason.is_none());
    let json = serde_json::to_string(&no_reason).unwrap();
    assert!(
        !json.contains("\"reason\""),
        "a None reason is skipped on the wire: {json}"
    );
}

/// reconstruct_reason returns the gate's protected-path ask for a Bash
/// command that runs a script from a discovered skill's directory, and
/// augment_skill_script_reason then replaces the generic detail with the
/// script's path so the approval card shows what would execute. The two
/// steps mirror handle_approval's order.
#[test]
fn test_reconstruct_skill_script() {
    use std::sync::Arc;
    let tmp = std::env::temp_dir().join(format!("recon-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&tmp));
    let skill_dir = tmp.join(".houyicoder").join("skills").join("deploy");
    std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: deploy skill\n---\nbody\n",
    )
    .unwrap();
    let reg: Arc<dyn houyicoder_api::skill::SkillRegistry> = Arc::new(
        crate::composition::SkillRegistryImpl::discover_with_home(Some(&tmp), None),
    );
    let sess_store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(houyicoder_provider::FakeProvider::text("test"));
    let runner = houyicoder_core::agent::Runner::with_shared_store(
        sess_store,
        provider,
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    )
    .with_skill_registry(reg);
    let server = super::Server::new(
        Arc::new(runner),
        houyicoder_context::SessionId::new(),
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let cmd = {
        let canon = std::fs::canonicalize(&skill_dir).unwrap();
        format!("python {}/scripts/deploy.py", canon.to_string_lossy())
    };
    let input = serde_json::json!({"command": cmd});
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
    std::fs::remove_dir_all(&tmp).ok();
}

/// format_skill_script_detail names the first script and counts the rest
/// when a command runs more than one skill script, so the card stays one
/// line instead of listing every script.
#[test]
fn test_format_multi_script_detail() {
    let scripts = vec![
        houyicoder_api::skill::SkillScriptRef {
            skill_name: "deploy".into(),
            script_rel_path: "scripts/a.py".into(),
        },
        houyicoder_api::skill::SkillScriptRef {
            skill_name: "deploy".into(),
            script_rel_path: "scripts/b.py".into(),
        },
    ];
    let detail = super::format_skill_script_detail(&scripts);
    assert!(detail.contains("2 skill scripts"), "count: {detail}");
    assert!(
        detail.contains("deploy/scripts/a.py"),
        "first named: {detail}"
    );
}

/// augment_skill_script_reason leaves the detail untouched for a non-shell
/// tool: the detection is for bash commands, so an edit/write ask keeps its
/// original reason. The same command on a bash ask does augment.
#[test]
fn test_augment_skips_non_shell() {
    use std::sync::Arc;
    let tmp = std::env::temp_dir().join(format!("aug-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&tmp));
    let skill_dir = tmp.join(".houyicoder").join("skills").join("deploy");
    std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: deploy skill\n---\nbody\n",
    )
    .unwrap();
    let reg: Arc<dyn houyicoder_api::skill::SkillRegistry> = Arc::new(
        crate::composition::SkillRegistryImpl::discover_with_home(Some(&tmp), None),
    );
    let sess_store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(houyicoder_provider::FakeProvider::text("test"));
    let runner = houyicoder_core::agent::Runner::with_shared_store(
        sess_store,
        provider,
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    )
    .with_skill_registry(reg);
    let server = super::Server::new(
        Arc::new(runner),
        houyicoder_context::SessionId::new(),
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let canon = std::fs::canonicalize(&skill_dir).unwrap();
    let input = serde_json::json!({"command": format!("python {}/scripts/deploy.py", canon.to_string_lossy())});

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
    std::fs::remove_dir_all(&tmp).ok();
}

/// Answering "always" must reach both layers: the fence makes this run
/// work, the store makes it survive a restart. Fence only and the grant is
/// forgotten next launch; store only and the run that just asked still
/// refuses the path. macOS-only: widening a live fence is Seatbelt-only.
#[cfg(target_os = "macos")]
#[test]
fn test_consent_reaches_both_layers() {
    use houyicoder_api::sandbox::SandboxSession;
    use houyicoder_permission::{FileRuleStore, RuleStore};
    use houyicoder_sandbox::PlatformSession;
    use std::sync::Arc;
    let root = std::env::temp_dir().join(format!("consent-dir-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let gate = Arc::new(houyicoder_permission::DefaultModeGate::new().with_store(store.clone()));
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let sess_store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(houyicoder_provider::FakeProvider::text("test"));
    let runner = houyicoder_core::agent::Runner::with_shared_store(
        sess_store,
        provider,
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    );
    let server = super::Server::new(Arc::new(runner), houyicoder_context::SessionId::new(), gate)
        .with_session(session.clone());
    let input = serde_json::json!({"path": outside.to_string_lossy(), "pattern": "x"});
    server.apply_consent_directory("grep", &input, "always");
    let coutside = std::fs::canonicalize(&outside).unwrap();
    let dirs = session.working_dirs();
    assert!(
        dirs.iter()
            .any(|d| std::path::Path::new(d.as_str()) == coutside.as_path()),
        "fence (additional_dirs) has the granted dir: {dirs:?}"
    );
    let stored = store.load_directories();
    assert!(
        stored.iter().any(|d| d == &coutside),
        "durable store persisted the granted dir: {stored:?}"
    );

    // A file (not a dir) path: add_working_dir fails (is_dir check) but
    // the consent must not crash — the eprintln surfaces the failure so
    // the user is not left in a silent death-loop.
    let file_path = root.join("not-a-dir.txt");
    std::fs::write(&file_path, b"x").expect("write file");
    let input = serde_json::json!({"path": file_path.to_string_lossy()});
    server.apply_consent_directory("grep", &input, "once");

    std::fs::remove_dir_all(&root).ok();
}

/// What the user authorized depends on WHY the gate asked, not on the tool.
/// A path-bounds ask is about a location, so always grants the directory; a
/// rule ask is about the tool, so always persists a rule and leaves the
/// fence alone. Keying off the tool name would widen the fence on an answer
/// that was never about a path. macOS-only: see the sibling test.
#[cfg(target_os = "macos")]
#[test]
fn test_ask_reason_selects_grant() {
    use houyicoder_api::sandbox::SandboxSession;
    use houyicoder_permission::{FileRuleStore, RuleStore};
    use houyicoder_sandbox::PlatformSession;
    use std::sync::Arc;
    let root = std::env::temp_dir().join(format!("route-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let gate = Arc::new(houyicoder_permission::DefaultModeGate::new().with_store(store.clone()));
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let sess_store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(houyicoder_provider::FakeProvider::text("test"));
    let runner = houyicoder_core::agent::Runner::with_shared_store(
        sess_store,
        provider,
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    );
    let server = super::Server::new(Arc::new(runner), houyicoder_context::SessionId::new(), gate)
        .with_session(session.clone());
    let input = serde_json::json!({"path": outside.to_string_lossy(), "pattern": "x"});

    // A grep approval whose ask reason is NOT path-bounds (e.g. a user-Ask
    // rule): no directory grant, scope "always" or not.
    let user_ask_reason = AskReason {
        source: AskSource::UserRule,
        validator: "some-rule",
        detail: "user rule fired".into(),
        containment_note: None,
    };
    server.route_consent("grep", &input, "always", Some(&user_ask_reason));
    assert!(
        store.load_directories().is_empty(),
        "non-path-bounds grep must not grant a directory"
    );

    // The same grep approval whose ask reason IS path-bounds: directory
    // granted to both layers.
    let path_bounds_reason = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds",
        detail: "path outside workspace".into(),
        containment_note: None,
    };
    server.route_consent("grep", &input, "always", Some(&path_bounds_reason));
    let coutside = std::fs::canonicalize(&outside).unwrap();
    assert!(
        store.load_directories().iter().any(|d| d == &coutside),
        "path-bounds grep grants the directory"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The blocker this fix closes: a batch that approves an outside grep
/// (path-bounds, grants the directory) then reaches a SECOND grep whose
/// path is now inside the granted directory. reconstruct_reason re-decides
/// that second call to Allow — reason None — and a None-reason route must
/// NOT fall through to apply_consent_rule, because the non-bash terminal
/// there is a contentless tool-level Allow rule (matches the tool
/// regardless of input, persisted at Project scope) that would silently
/// install a permanent blanket grep allow, shadowing every later
/// path-bounds ask across restarts. None means the consent authority does
/// not know why the gate asked; it must persist nothing.
#[test]
fn test_none_reason_no_rule() {
    use houyicoder_api::sandbox::SandboxSession;
    use houyicoder_permission::{Effect, FileRuleStore, RuleStore};
    use houyicoder_sandbox::PlatformSession;
    use std::sync::Arc;
    let root = std::env::temp_dir().join(format!("none-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let outside = root.join("outside");
    let nested = outside.join("sub");
    std::fs::create_dir_all(&nested).expect("mkdir nested");
    let store: Arc<dyn RuleStore> = Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ));
    let gate = Arc::new(houyicoder_permission::DefaultModeGate::new().with_store(store.clone()));
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    let sess_store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(houyicoder_provider::FakeProvider::text("test"));
    let runner = houyicoder_core::agent::Runner::with_shared_store(
        sess_store,
        provider,
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    );
    let server = super::Server::new(Arc::new(runner), houyicoder_context::SessionId::new(), gate)
        .with_session(session.clone());

    // First approval in the batch: path-bounds, scope "always" grants the
    // outside directory to the fence + store.
    let outside_input = serde_json::json!({"path": outside.to_string_lossy(), "pattern": "x"});
    let path_bounds = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds",
        detail: "path outside workspace".into(),
        containment_note: None,
    };
    server.route_consent("grep", &outside_input, "always", Some(&path_bounds));

    // Second approval: path is now inside the granted directory, so the
    // re-decide returns Allow — reason None. This is the regression
    // surface: a None must not reach apply_consent_rule.
    let nested_input = serde_json::json!({"path": nested.to_string_lossy(), "pattern": "y"});
    server.route_consent("grep", &nested_input, "always", None);

    let blanket_grep_allow = store
        .load()
        .iter()
        .any(|r| r.action == "grep" && r.content.is_none() && r.effect == Effect::Allow);
    assert!(
        !blanket_grep_allow,
        "None-reason grep must not install a contentless blanket allow rule: {:?}",
        store.load()
    );

    std::fs::remove_dir_all(&root).ok();
}

/// A minimal server for the ask-wait tests: a stub runner + a default gate.
/// handle_approval only touches the runner's store (audit append) + the gate
/// (reconstruct_reason), so no sandbox session is needed.
fn ask_wait_server() -> super::Server {
    use houyicoder_api::provider::ModelProvider;
    use std::sync::Arc;
    let sess_store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn ModelProvider> =
        Arc::new(houyicoder_provider::FakeProvider::text("test"));
    let runner = houyicoder_core::agent::Runner::with_shared_store(
        sess_store,
        provider,
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    );
    super::Server::new(
        Arc::new(runner),
        houyicoder_context::SessionId::new(),
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
}

fn encode_line(msg: &impl serde::Serialize) -> String {
    let mut f = houyicoder_protocol::framing::encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    f
}

/// A non-matching frame mid-ask (a Status Request) is dropped, not fatal;
/// the ask-wait keeps waiting and pairs the real permission response that
/// follows. Pins the root-cause fix at the lib (effect) level.
#[tokio::test]
async fn test_handle_drops_non_matching() {
    use futures::SinkExt;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use houyicoder_protocol::envelope::{
        ClientFrame, ClientResponseEnvelope, ClientResponsePayload, RequestEnvelope, RequestId,
        ServerFrame,
    };
    use houyicoder_protocol::frontend::FrontendRequest;
    use houyicoder_protocol::frontend::run::ApprovalDecision;

    let mut server = ask_wait_server();
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = super::ServerIo::new(server_tx, server_rx);
    let approval = houyicoder_core::agent::ApprovalRequest::new(
        "c1".into(),
        "bash".into(),
        serde_json::json!({"command": "echo hi"}),
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
    use futures::SinkExt;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use houyicoder_protocol::acp_wire::AcpNotification;
    use houyicoder_protocol::envelope::ServerFrame;

    let mut server = ask_wait_server();
    let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
    let mut io = super::ServerIo::new(server_tx, server_rx);
    let approval = houyicoder_core::agent::ApprovalRequest::new(
        "c1".into(),
        "bash".into(),
        serde_json::json!({"command": "echo hi"}),
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
        let cancel = AcpNotification::new("session/cancel", serde_json::json!({}));
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
