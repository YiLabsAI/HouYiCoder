//! Session lifecycle: handshake, prompt driving, and cancellation.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use houyicoder_api::launcher::{LauncherChild, ProcessLauncher, SpawnPolicy, SpawnRequest};
#[cfg(test)]
use houyicoder_api::launcher::{LauncherExit, SpawnError};
use houyicoder_api::session::SessionLog;
use houyicoder_async::CancellationToken;
use houyicoder_context::SessionId;
use houyicoder_core::agent::multi_agent::registry::ExternalAcpSpec;
use houyicoder_protocol::acp_wire::{
    AcpNotification, AcpRequest, AcpRequestId, AcpResponse, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::run::ContentBlock;
use tokio::time::timeout;

use super::transcript::TranscriptFold;
use crate::acp::pump::{AcpDuplexPump, InboundFrame, spawn_pump};

const CANCEL_GRACE: Duration = Duration::from_millis(500);
const MAX_BANNER_LINES: usize = 32;

#[derive(Debug)]
pub enum AcpClientError {
    ConstructionFailed { program: String },
    HandshakeTimedOut,
    ChildExited(String),
    Protocol(ProtocolError),
}

impl Display for AcpClientError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConstructionFailed { program } => {
                write!(f, "could not start program {program}")
            }
            Self::HandshakeTimedOut => write!(f, "handshake timed out"),
            Self::ChildExited(msg) => write!(f, "child exited: {msg}"),
            Self::Protocol(e) => write!(f, "protocol error: {e}"),
        }
    }
}

impl Error for AcpClientError {}

impl From<ProtocolError> for AcpClientError {
    fn from(e: ProtocolError) -> Self {
        Self::Protocol(e)
    }
}

fn protocol_err(msg: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCategory::InvalidFrame, msg, false)
}

pub struct AcpChildSession {
    pump: AcpDuplexPump,
    child: LauncherChild,
    session: SessionId,
    next_req_id: i64,
}

impl AcpChildSession {
    pub fn connect(
        launcher: &Arc<dyn ProcessLauncher>,
        spec: &ExternalAcpSpec,
        workspace: &Path,
        _host_env: &[(String, String)],
    ) -> Result<Self, AcpClientError> {
        let req = SpawnRequest::new(spec.program.clone())
            .with_args(spec.args.clone())
            .with_workspace(workspace.to_path_buf())
            .interactive();
        let policy = SpawnPolicy::default().sandboxed();
        let mut child =
            launcher
                .spawn(req, policy)
                .map_err(|_| AcpClientError::ConstructionFailed {
                    program: spec.program.clone(),
                })?;
        let pipes = child
            .take_pipes()
            .ok_or_else(|| AcpClientError::ConstructionFailed {
                program: spec.program.clone(),
            })?;
        let stdin = pipes
            .stdin
            .ok_or_else(|| AcpClientError::ConstructionFailed {
                program: spec.program.clone(),
            })?;
        let stdout = pipes
            .stdout
            .ok_or_else(|| AcpClientError::ConstructionFailed {
                program: spec.program.clone(),
            })?;
        let pump = spawn_pump(stdout, stdin);
        Ok(Self {
            pump,
            child,
            session: SessionId::new(),
            next_req_id: 1,
        })
    }

    pub async fn handshake(
        &mut self,
        spec: &ExternalAcpSpec,
        workspace: &Path,
    ) -> Result<SessionId, AcpClientError> {
        let budget = Duration::from_millis(spec.handshake_timeout_ms);
        timeout(budget, self.handshake_inner(workspace))
            .await
            .map_err(|_| AcpClientError::HandshakeTimedOut)?
    }

    async fn handshake_inner(&mut self, workspace: &Path) -> Result<SessionId, AcpClientError> {
        let init_id = self.next_request_id();
        self.send_request(init_id, "initialize", &serde_json::json!({}))
            .await?;
        let resp = self.await_response(init_id).await?;
        let _init: InitializeResponse =
            serde_json::from_value(resp).map_err(|e| protocol_err(format!("bad init: {e}")))?;
        let new_params = serde_json::to_value(NewSessionRequest {
            cwd: workspace.display().to_string(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            meta: None,
        })
        .expect("session/new serializes");
        let new_id = self.next_request_id();
        self.send_request(new_id, "session/new", &new_params)
            .await?;
        let resp = self.await_response(new_id).await?;
        let session_data: NewSessionResponse =
            serde_json::from_value(resp).map_err(|e| protocol_err(format!("bad new: {e}")))?;
        self.session = SessionId::from_display_string(&session_data.session_id).unwrap_or_default();
        Ok(self.session)
    }

    pub async fn drive_prompt(
        &mut self,
        session: SessionId,
        prompt: &str,
        log: &Arc<dyn SessionLog>,
        cancel: &CancellationToken,
    ) -> Result<String, AcpClientError> {
        let prompt_id = self.next_request_id();
        let params = serde_json::to_value(PromptRequest {
            session_id: session.to_string(),
            prompt: vec![ContentBlock::Text {
                text: prompt.into(),
            }],
            meta: None,
        })
        .expect("prompt serializes");
        self.send_request(prompt_id, "session/prompt", &params)
            .await?;
        let mut fold = TranscriptFold::new(session, log.clone());
        let cancel_fut = cancel.cancelled();
        tokio::pin!(cancel_fut);
        tokio::select! {
            biased;
            _ = &mut cancel_fut => {
                self.send_notification("session/cancel", &serde_json::json!({})).await?;
                match timeout(CANCEL_GRACE, self.recv_until_prompt(prompt_id, &mut fold)).await {
                    Ok(Ok(stop)) => Ok(stop),
                    _ => Err(AcpClientError::ChildExited("cancelled and killed".into())),
                }
            }
            result = self.recv_until_prompt(prompt_id, &mut fold) => {
                result
            }
        }
    }

    async fn recv_until_prompt(
        &mut self,
        prompt_id: i64,
        fold: &mut TranscriptFold,
    ) -> Result<String, AcpClientError> {
        loop {
            let Some(frame) = self.pump.next_frame().await else {
                return Err(AcpClientError::ChildExited("pipe closed".into()));
            };
            match frame {
                InboundFrame::Notification(notif) => {
                    fold.handle_notification(&notif)
                        .await
                        .map_err(|e| AcpClientError::Protocol(protocol_err(e)))?;
                }
                InboundFrame::Response(resp) => {
                    let resp_id = match &resp {
                        AcpResponse::Result { id, .. } | AcpResponse::Error { id, .. } => {
                            id.clone()
                        }
                    };
                    if let AcpRequestId::Number(n) = resp_id
                        && n == prompt_id
                    {
                        let result = match resp {
                            AcpResponse::Result { result, .. } => result,
                            AcpResponse::Error { error, .. } => {
                                return Err(AcpClientError::Protocol(protocol_err(format!(
                                    "child error: {:?}",
                                    error.code
                                ))));
                            }
                        };
                        let pr: PromptResponse = serde_json::from_value(result)
                            .map_err(|e| protocol_err(format!("bad prompt response: {e}")))?;
                        return Ok(format!("{:?}", pr.stop_reason));
                    }
                }
                InboundFrame::Request(req) => {
                    tracing::warn!(
                        method = %req.method,
                        "external child sent an unexpected reverse request; ignoring"
                    );
                }
                InboundFrame::Unparseable(line) => {
                    tracing::warn!(bytes = line.len(), "unparseable line from child");
                }
            }
        }
    }

    fn next_request_id(&mut self) -> i64 {
        let id = self.next_req_id;
        self.next_req_id += 1;
        id
    }

    async fn send_request(
        &mut self,
        id: i64,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<(), ProtocolError> {
        let req = AcpRequest::new(id, method, params.clone());
        let frame = encode(&req)?;
        self.pump.send_frame(frame).await
    }

    async fn send_notification(
        &mut self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<(), ProtocolError> {
        let notif = AcpNotification::new(method, params.clone());
        let frame = encode(&notif)?;
        self.pump.send_frame(frame).await
    }

    async fn await_response(&mut self, id: i64) -> Result<serde_json::Value, AcpClientError> {
        let mut response = self.pump.expect_response(id);
        let mut banners = 0;
        loop {
            tokio::select! {
                resp = &mut response => {
                    let resp = resp.map_err(|_| AcpClientError::ChildExited(
                        "response waiter dropped".into(),
                    ))?;
                    return Ok(match resp {
                        AcpResponse::Result { result, .. } => result,
                        AcpResponse::Error { error, .. } => {
                            return Err(AcpClientError::Protocol(protocol_err(
                                format!("child error: {:?}", error.code),
                            )));
                        }
                    });
                }
                frame = self.pump.next_frame() => {
                    match frame {
                        Some(InboundFrame::Response(resp)) => {
                            self.pump.deliver_response(resp);
                        }
                        Some(InboundFrame::Unparseable(line)) => {
                            banners += 1;
                            tracing::debug!(line = %line, "skipped banner line during handshake");
                            if banners > MAX_BANNER_LINES {
                                return Err(AcpClientError::Protocol(protocol_err(
                                    "too many non-JSON lines during handshake",
                                )));
                            }
                        }
                        Some(_) => {}
                        None => {
                            return Err(AcpClientError::ChildExited(
                                "child closed during handshake".into(),
                            ));
                        }
                    }
                }
            }
        }
    }

    pub async fn shutdown(self) {
        drop(self.pump);
        self.child.wait().await.ok();
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    #[cfg(test)]
    fn from_pump(pump: AcpDuplexPump) -> Self {
        let waiter: houyicoder_async::PFut<'static, Result<LauncherExit, SpawnError>> =
            Box::pin(async {
                Ok(LauncherExit {
                    exit_code: Some(0),
                    stdout: None,
                    stderr: None,
                })
            });
        Self {
            pump,
            child: LauncherChild::new(None, waiter),
            session: SessionId::new(),
            next_req_id: 1,
        }
    }
}

#[cfg(test)]
mod tests;
