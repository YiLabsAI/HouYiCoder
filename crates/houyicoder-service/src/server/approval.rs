//! The mid-run permission reverse-request: the serve loop surfaced an
//! ApprovalRequest (the gate returned Ask), so the server pauses the run,
//! asks the human over the wire, records the verdict audit, applies any
//! yes-don't-ask-again consent, and returns the engine decision for resume.
//! Split from server.rs so that file stays under the file-size gate.

use houyicoder_context::{EventId, PermissionVerdict, TurnEvent, TurnEventKind};
use houyicoder_permission::{Decision, ToolRequest};
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::wire::{WireError, WireErrorKind};

use crate::projection::{build_approval_request, parse_approval_decision};

use super::{Server, io::ServerIo, now_millis};

impl Server {
    /// Drive one approval request to a human answer. Reconstructs the Ask
    /// reason the gate produced by re-running the ladder with the request the
    /// engine surfaced (the gate already decided Ask to get here, so the
    /// re-decide is a display-only reconstruction; no state mutates between the
    /// two calls except an intervening /mode or rule change, which is an
    /// accepted race (the state can change between the check and the use). Sends the wire ask, reads the
    /// matching reverse response, records the durable verdict audit, applies
    /// a scoped consent rule on yes-always, advances the parked-turn cursor,
    /// and returns the engine decision the runner resumes with.
    pub(super) async fn handle_approval(
        &mut self,
        io: &mut ServerIo,
        approval: &houyicoder_core::agent::ApprovalRequest,
    ) -> Result<houyicoder_core::agent::ApprovalDecision, WireError> {
        let reason = self.reconstruct_reason(&approval.tool_name, &approval.input);

        let ask_id = self.mint_req_id();
        let ask = ServerRequestEnvelope::new(
            ask_id,
            ServerRequestPayload::Permission(build_approval_request(approval, reason.as_ref())),
        );
        self.send_typed(io, &ServerFrame::Request(ask)).await?;
        // Read the matching reverse response. Loop, not a single read, so a
        // non-matching frame mid-ask does not fatal: a reconnecting client's
        // first status tick, a racing poll, a session/cancel, or a mode-cycle
        // all land here while the ask is in flight. The prior single read +
        // fatal arm returned InvalidFrame and closed the connection, so the run
        // never resumed (the AskUserQuestion deadlock). Aligns with the mid-run
        // and mid-resume selects: dispatch session/* notifications + mode-cycle
        // requests, drop the rest, fatal only on client close.
        let resp = loop {
            let frame = match io.next_frame().await {
                Some(f) => f,
                None => {
                    self.flush_pushed_count();
                    return Err(WireError::new(
                        WireErrorKind::Unavailable,
                        "client closed mid-permission",
                        false,
                    ));
                }
            };
            if let Ok(notif) =
                serde_json::from_str::<houyicoder_protocol::acp_wire::AcpNotification>(&frame)
            {
                let is_cancel = notif.method == "session/cancel";
                self.handle_session_notification(&notif);
                if is_cancel {
                    // Esc: abort + return a deny so the serve loop resumes the
                    // now-cancelled run instead of hanging on a response the
                    // client will not send.
                    return Ok(houyicoder_core::agent::ApprovalDecision {
                        call_id: approval.call_id.clone(),
                        approved: false,
                        updated_input: None,
                    });
                }
                continue;
            }
            if let Ok(ClientFrame::Response(r)) = serde_json::from_str::<ClientFrame>(&frame)
                && r.req_id == ask_id
            {
                break r;
            }
            // A non-matching frame (a Request, a mismatched Response, or an
            // unparseable frame) is dropped so the ask stays open. Surface it
            // so a protocol-incompatible client does not look stuck with no
            // clue (the old fatal arm at least logged the InvalidFrame).
            tracing::warn!(
                "ask-wait: dropped a non-matching frame while waiting for the permission response (first 80 chars): {}",
                frame.chars().take(80).collect::<String>()
            );
        };
        let decision = match resp.payload {
            ClientResponsePayload::Permission(d) => d,
            // non_exhaustive guard: a future reverse-response shape. The
            // reverse-request flow only asks Permission today.
            _ => {
                return Err(WireError::new(
                    WireErrorKind::InvalidFrame,
                    "expected a permission reverse response",
                    false,
                ));
            }
        };
        // Record the durable PermissionDecision audit event before resume
        // applies the decision. The engine resume path appends only the
        // ToolResult, never the verdict, so the verdict trail would be lost
        // over the wire without this append. The scope the client chose rides
        // the wire decision; the tool name + call_id come from the ask the
        // server sent.
        let verdict = if decision.approved {
            PermissionVerdict::Approved
        } else {
            PermissionVerdict::Denied
        };
        let audit = TurnEvent {
            id: EventId::new(),
            session: self.session,
            ts: now_millis(),
            prev_hash: None,
            kind: TurnEventKind::PermissionDecision {
                call_id: approval.call_id.clone(),
                tool: approval.tool_name.clone(),
                verdict,
                scope: decision.scope.clone(),
            },
        };
        if let Err(e) = self.runner.store().append(audit).await {
            // Best-effort audit: a failed append does not fail the run (the
            // verdict still reaches the engine via resume), only the durable
            // trail.
            tracing::warn!("permission-decision audit append failed: {e}");
        }
        // Route the consent by the ASK REASON, not the tool name: a path-bounds
        // approval grants the directory (apply_consent_directory); anything
        // else takes the rule path on scope "always". Shared with resume_pending
        // via route_consent so the two consent sites cannot drift.
        if decision.approved {
            self.route_consent(
                &approval.tool_name,
                &approval.input,
                &decision.scope,
                reason.as_ref(),
            );
        }
        // Move this ask from remaining into decided so a mid-batch disconnect
        // re-emits only the tail and the runner resumes with the full decided
        // set.
        if let Some(host) = &self.host {
            host.store().advance_pending(self.session, decision.clone());
        }
        Ok(parse_approval_decision(decision))
    }

    /// Re-run the ladder to reconstruct the Ask reason the gate attached when
    /// it surfaced this approval, so the consent router can decide WHICH
    /// durable authorization (directory grant vs rule) to apply. The gate
    /// already decided Ask to get here; this re-decide is best-effort: state
    /// CAN shift between the original ask and this call — a prior approval in
    /// the same batch may have granted a directory to the fence, a /mode may
    /// have cycled, a rule may have been added — and a shift can return Allow
    /// (reason None) for a call the gate originally asked on. route_consent
    /// treats None as fail-closed (persist nothing) precisely because this
    /// re-decide is not authoritative. The carry-reason follow-up threads the
    /// original AskReason through the engine's ApprovalRequest so the consent
    /// path stops re-deciding and reads the reason the gate actually produced;
    /// that also removes the metrics double-count this re-decide incurs.
    pub(crate) fn reconstruct_reason(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Option<houyicoder_permission::AskReason> {
        // is_destructive / is_read_only are unused by the ladder (no validator
        // reads them); native_requires_approval is set true so a mode-default
        // ToolNative ask reproduces. A rule, safety, or detection ask fires
        // before the mode default regardless of the flag, so every Ask path
        // reconstructs.
        let req = ToolRequest {
            tool_name,
            input: Some(input),
            is_destructive: false,
            is_read_only: false,
            native_requires_approval: true,
        };
        match self.gate.decide(&req) {
            Decision::Ask(r) => Some(r),
            // Defensive: the engine only surfaces an approval when the gate
            // said Ask, so reaching Allow / Deny here means mode or rule state
            // shifted between the ask and this reconstruction. The ask still
            // goes out; the card renders a generic prompt.
            _ => None,
        }
    }

    /// Route an approval's consent by the ASK REASON, not the tool name: only a
    /// path-bounds ask grants a directory (apply_consent_directory); an ask
    /// with any OTHER reason takes the rule path (apply_consent_rule) on scope
    /// "always". A None reason — the re-decide could not reproduce why the gate
    /// asked — fails closed: nothing is persisted. Routing by tool name was
    /// wrong (any approved grep/glob got a directory grant even when the ask
    /// was a user-Ask rule), and routing a None reason to the rule path was
    /// worse: apply_consent_rule's non-bash terminal is a contentless
    /// tool-level Allow rule (matches the tool regardless of input, persisted
    /// at Project scope), so a batch that grants a directory then re-decides a
    /// nested path to Allow would silently install a permanent blanket grep
    /// allow that shadows all later path-bounds asks. None means "the consent
    /// authority does not know why this ask happened"; writing any durable
    /// authorization in that state would grant a permission the user never
    /// chose. Shared by handle_approval + resume_pending so the two consent
    /// sites cannot drift. The carry-reason follow-up threads the original
    /// AskReason through the engine's ApprovalRequest so this re-decide (and
    /// its None case) disappears from the consent path.
    pub(crate) fn route_consent(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        scope: &str,
        reason: Option<&houyicoder_permission::AskReason>,
    ) {
        let is_path_bounds = reason.is_some_and(|r| r.validator == "path-bounds");
        if is_path_bounds {
            self.apply_consent_directory(tool_name, input, scope);
        } else if reason.is_some() && scope == "always" {
            self.apply_consent_rule(tool_name, input);
        }
        // reason == None: fail closed. Persist nothing.
    }

    /// Route a path-bounds approval (grep/glob) to the two persistence layers:
    /// the kernel fence (additional_dirs, always — so the gate's re-check on
    /// resume passes instead of re-asking in a loop) and the durable store
    /// (only on scope "always", so the fence rehydrates the directory on
    /// restart). Mirrors apply_consent_rule for bash: same scope contract
    /// ("always" = persist, "once" = this session), reusing Scope rather than
    /// a new concept. The path extraction is the shared path_args_for_boundary
    /// (the gate uses the same helper for its pre-check ask) so the two layers
    /// cannot drift on which field is the path.
    pub(crate) fn apply_consent_directory(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        scope: &str,
    ) {
        let paths = houyicoder_api::sandbox::path_args_for_boundary(tool_name, Some(input));
        for p in paths {
            if let Some(session) = &self.sandbox_session
                && let Err(e) = session.add_working_dir(&p)
            {
                tracing::warn!(
                    "path-bounds consent: add_working_dir failed for {p}: {e}; the tool will still refuse the path"
                );
            }
            if scope == "always" {
                self.gate.add_directory(
                    std::path::Path::new(&p),
                    houyicoder_permission::Scope::Local,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
        let wired = build_approval_request(&req, Some(&reason));
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
        let no_reason = build_approval_request(&req, None);
        assert!(no_reason.reason.is_none());
        let json = serde_json::to_string(&no_reason).unwrap();
        assert!(
            !json.contains("\"reason\""),
            "a None reason is skipped on the wire: {json}"
        );
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
        let gate =
            Arc::new(houyicoder_permission::DefaultModeGate::new().with_store(store.clone()));
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
        let server =
            super::Server::new(Arc::new(runner), houyicoder_context::SessionId::new(), gate)
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
        let gate =
            Arc::new(houyicoder_permission::DefaultModeGate::new().with_store(store.clone()));
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
        let server =
            super::Server::new(Arc::new(runner), houyicoder_context::SessionId::new(), gate)
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
        let gate =
            Arc::new(houyicoder_permission::DefaultModeGate::new().with_store(store.clone()));
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
        let server =
            super::Server::new(Arc::new(runner), houyicoder_context::SessionId::new(), gate)
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
            .handle_approval(&mut io, &approval)
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
            .handle_approval(&mut io, &approval)
            .await
            .expect("handle_approval returned on cancel");
        feeder.await.expect("feeder done");
        assert!(
            !decision.approved,
            "cancel mid-ask returns a deny, not a hang"
        );
    }
}
