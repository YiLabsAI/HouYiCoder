//! The startup workspace-trust reverse-request. Mirrors the mid-run
//! permission ask (server/approval.rs) but fires once before the frame loop:
//! when the project workspace is not yet trusted, the server asks the client
//! to confirm, persists the answer in user-level settings so the prompt does
//! not repeat, and shuts the session down on a decline. Split from server.rs
//! to keep that file under the size gate.

use houyicoder_api::trust::TrustState;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::frontend::trust::TrustPrompt;
use houyicoder_protocol::wire::{WireError, WireErrorKind};

use super::Server;

/// Resolve the trust state for a project path from persisted user-level
/// settings. Acknowledged when the path or an ancestor is recorded as
/// trusted; Untrusted otherwise. User-level (never project-local) so a
/// repository cannot self-author trust. A None project path (a non-project
/// session, e.g. a test harness) is Trusted — no project source to gate.
pub(crate) fn compute_trust_state(
    settings_path: &std::path::Path,
    project_path: Option<&std::path::Path>,
) -> TrustState {
    let Some(path) = project_path else {
        return TrustState::Trusted;
    };
    if houyicoder_config::is_path_trusted(settings_path, path) {
        TrustState::Acknowledged
    } else {
        TrustState::Untrusted
    }
}

impl Server {
    /// Ask the client to trust the project workspace when it is not yet
    /// acknowledged. Fires once at serve start (before the frame loop), so
    /// no run proceeds until trust is resolved. On accept the path is
    /// persisted in user-level settings so the prompt never repeats for it
    /// or its descendants; on decline the session ends (no partial trust).
    /// A None project path skips the prompt (a non-project session).
    pub(crate) async fn ensure_trust(
        &mut self,
        io: &mut super::io::ServerIo,
    ) -> Result<TrustState, WireError> {
        // Clone the path + settings path into owned locals so no shared
        // borrow of self spans the mutable send_typed / mint_req_id calls.
        let project_path = match self.project_path.as_deref() {
            Some(p) => p.to_path_buf(),
            None => return Ok(TrustState::Trusted),
        };
        let settings_path = self.settings_path.clone();
        let state = compute_trust_state(&settings_path, Some(&project_path));
        if state != TrustState::Untrusted {
            return Ok(state);
        }
        let ask_id = self.mint_req_id();
        let prompt = ServerRequestEnvelope::new(
            ask_id,
            ServerRequestPayload::TrustPrompt(TrustPrompt {
                project_path: project_path.to_string_lossy().into_owned(),
                risks: Vec::new(),
            }),
        );
        self.send_typed(io, &ServerFrame::Request(prompt)).await?;
        // Mirror the permission-ask wait: loop over incoming frames so a
        // non-matching frame mid-ask (a status tick, a session/cancel) does
        // not fatal. session/cancel mid-trust is a decline.
        let resp = loop {
            let frame = match io.next_frame().await {
                Some(f) => f,
                None => {
                    self.flush_pushed_count();
                    return Err(WireError::new(
                        WireErrorKind::Unavailable,
                        "client closed mid-trust-prompt",
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
                    return Err(WireError::new(
                        WireErrorKind::Unavailable,
                        "user declined trust (cancel)",
                        false,
                    ));
                }
                continue;
            }
            if let Ok(ClientFrame::Response(r)) = serde_json::from_str::<ClientFrame>(&frame)
                && r.req_id == ask_id
            {
                break r;
            }
            tracing::warn!(
                "trust-wait: dropped a non-matching frame while waiting for the trust response (first 80 chars): {}",
                frame.chars().take(80).collect::<String>()
            );
        };
        let accept = match resp.payload {
            ClientResponsePayload::TrustAccept(a) => a,
            _ => {
                return Err(WireError::new(
                    WireErrorKind::InvalidFrame,
                    "expected a trust_accept reverse response",
                    false,
                ));
            }
        };
        if !accept.accepted {
            return Err(WireError::new(
                WireErrorKind::Unavailable,
                "user declined to trust the project",
                false,
            ));
        }
        // Persist the accepted path so the prompt does not repeat for it or
        // its descendants. A persist failure does not fail the session — the
        // user already accepted this run, so trust holds in session memory;
        // only the next launch re-asks.
        if let Err(e) = houyicoder_config::persist_project_trust(&settings_path, &project_path) {
            tracing::warn!("trust persist failed (re-asks next launch): {e:?}");
        }
        Ok(TrustState::Acknowledged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project path not recorded in settings is Untrusted; recording it
    /// flips it to Acknowledged (persisted in user-level settings). A None
    /// project path is Trusted (a non-project session, no project source).
    #[test]
    fn test_compute_trust_state() {
        let dir = std::env::temp_dir().join(format!("trust-compute-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();

        assert_eq!(
            compute_trust_state(&settings, Some(&proj)),
            TrustState::Untrusted,
            "unrecorded project is untrusted"
        );
        houyicoder_config::persist_project_trust(&settings, &proj).unwrap();
        assert_eq!(
            compute_trust_state(&settings, Some(&proj)),
            TrustState::Acknowledged,
            "recorded project is acknowledged"
        );

        // None project path: a non-project session, trusted (nothing to gate).
        assert_eq!(
            compute_trust_state(&settings, None),
            TrustState::Trusted,
            "no project path is trusted"
        );

        drop(std::fs::remove_dir_all(&dir));
    }

    /// A minimal server bound to a temp settings path + a project path that
    /// is not yet trusted, so ensure_trust fires the prompt. Mirrors the
    /// ask_wait_server helper in server/approval.rs.
    fn trust_prompt_server(
        settings: std::path::PathBuf,
        project: std::path::PathBuf,
    ) -> super::Server {
        use std::sync::Arc;
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
        super::Server::new(
            Arc::new(runner),
            houyicoder_context::SessionId::new(),
            Arc::new(houyicoder_permission::DefaultModeGate::new()),
        )
        .with_settings_path(settings)
        .with_project_path(project)
    }

    fn encode_line(msg: &impl serde::Serialize) -> String {
        let mut f = houyicoder_protocol::framing::encode(msg).expect("encode");
        if !f.ends_with('\n') {
            f.push('\n');
        }
        f
    }

    /// An untrusted project fires the TrustPrompt; the client accepts; the
    /// path is persisted in user-level settings and ensure_trust returns
    /// Acknowledged (so the prompt will not repeat next launch).
    #[tokio::test]
    async fn test_ensure_trust_accepts() {
        use futures::SinkExt;
        use futures::StreamExt;
        use futures::channel::mpsc;
        use houyicoder_protocol::envelope::{
            ClientFrame, ClientResponseEnvelope, ClientResponsePayload, ServerFrame,
        };

        let dir = std::env::temp_dir().join(format!("trust-accept-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();

        let mut server = trust_prompt_server(settings.clone(), proj.clone());
        let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
        let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
        let mut io = super::super::io::ServerIo::new(server_tx, server_rx);

        let feeder = tokio::spawn(async move {
            // Read the TrustPrompt ask to learn its req_id, then accept.
            let mut ask_id = None;
            for _ in 0..16 {
                let line = client_rx.next().await.expect("server frame");
                if let Ok(ServerFrame::Request(req)) = serde_json::from_str(&line) {
                    ask_id = Some(req.req_id);
                    break;
                }
            }
            let ask_id = ask_id.expect("trust prompt sent");
            let resp = ClientFrame::Response(ClientResponseEnvelope::new(
                ask_id,
                ClientResponsePayload::TrustAccept(
                    houyicoder_protocol::frontend::trust::TrustAccept { accepted: true },
                ),
            ));
            client_tx
                .send(encode_line(&resp))
                .await
                .expect("send accept");
        });

        let state = server
            .ensure_trust(&mut io)
            .await
            .expect("accept resolves trust");
        feeder.await.expect("feeder done");
        assert_eq!(state, TrustState::Acknowledged, "accept acknowledges");
        // The path is now persisted so the next session skips the prompt.
        assert!(
            houyicoder_config::is_path_trusted(&settings, &proj),
            "accept persists the path"
        );

        drop(std::fs::remove_dir_all(&dir));
    }

    /// A decline ends the session: ensure_trust returns Err, and the path is
    /// NOT persisted (no partial trust, no silent acceptance).
    #[tokio::test]
    async fn test_ensure_trust_declines() {
        use futures::SinkExt;
        use futures::StreamExt;
        use futures::channel::mpsc;
        use houyicoder_protocol::envelope::{
            ClientFrame, ClientResponseEnvelope, ClientResponsePayload, ServerFrame,
        };

        let dir = std::env::temp_dir().join(format!("trust-decline-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();

        let mut server = trust_prompt_server(settings.clone(), proj.clone());
        let (mut client_tx, server_rx) = mpsc::channel::<String>(8);
        let (server_tx, mut client_rx) = mpsc::channel::<String>(8);
        let mut io = super::super::io::ServerIo::new(server_tx, server_rx);

        let feeder = tokio::spawn(async move {
            let mut ask_id = None;
            for _ in 0..16 {
                let line = client_rx.next().await.expect("server frame");
                if let Ok(ServerFrame::Request(req)) = serde_json::from_str(&line) {
                    ask_id = Some(req.req_id);
                    break;
                }
            }
            let ask_id = ask_id.expect("trust prompt sent");
            let resp = ClientFrame::Response(ClientResponseEnvelope::new(
                ask_id,
                ClientResponsePayload::TrustAccept(
                    houyicoder_protocol::frontend::trust::TrustAccept { accepted: false },
                ),
            ));
            client_tx
                .send(encode_line(&resp))
                .await
                .expect("send decline");
        });

        let result = server.ensure_trust(&mut io).await;
        feeder.await.expect("feeder done");
        assert!(result.is_err(), "decline ends the session");
        assert!(
            !houyicoder_config::is_path_trusted(&settings, &proj),
            "decline must not persist trust"
        );

        drop(std::fs::remove_dir_all(&dir));
    }
}
