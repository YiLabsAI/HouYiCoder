//! Session lifecycle: handshake, prompt driving, and cancellation.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use houyicoder_api::launcher::{LauncherChild, ProcessLauncher, SpawnPolicy, SpawnRequest};
#[cfg(test)]
use houyicoder_api::launcher::{LauncherExit, SpawnError};
use houyicoder_api::session::SessionLog;
use houyicoder_async::CancellationToken;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::SessionId;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_request_topic, permission_response_topic,
};
use houyicoder_core::agent::multi_agent::registry::ExternalAcpSpec;
use houyicoder_permission::ModeGate;
use houyicoder_protocol::acp_wire::{
    AcpNotification, AcpRequest, AcpRequestId, AcpResponse, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::run::ContentBlock;
use serde_json::Value;
use tokio::time::{sleep, timeout};

use super::permission::{check_fence, check_gate, verdict_response};
use super::transcript::TranscriptFold;
use crate::acp::pump::{AcpDuplexPump, InboundFrame, spawn_pump};

const CANCEL_GRACE: Duration = Duration::from_millis(500);
const MAX_BANNER_LINES: usize = 32;
/// How long a delegated ask waits for the parent user before failing closed.
const PARENT_ASK_TIMEOUT: Duration = Duration::from_secs(120);

/// The parent-side context one external child's permission asks run under.
pub struct PermissionFence {
    /// The workspace root the child is confined to.
    pub workspace_root: PathBuf,
    /// The parent's gate, consulted before delegating to the user.
    pub gate: Arc<dyn ModeGate>,
    /// The multi-agent bus to publish delegated asks on.
    pub bus: Option<Arc<AgentBus>>,
    /// The child identity delegated asks carry.
    pub child_id: String,
    /// The agent type delegated asks report.
    pub agent_type: String,
}

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
        fence: Option<&PermissionFence>,
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
                match timeout(CANCEL_GRACE, self.recv_until_prompt(prompt_id, &mut fold, fence)).await {
                    Ok(Ok(stop)) => Ok(stop),
                    _ => Err(AcpClientError::ChildExited("cancelled and killed".into())),
                }
            }
            result = self.recv_until_prompt(prompt_id, &mut fold, fence) => {
                result
            }
        }
    }

    async fn recv_until_prompt(
        &mut self,
        prompt_id: i64,
        fold: &mut TranscriptFold,
        fence: Option<&PermissionFence>,
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
                    if req.method == "session/request_permission"
                        && let Some(fence) = fence
                    {
                        self.answer_child_permission(&req, fence).await?;
                    } else {
                        tracing::warn!(
                            method = %req.method,
                            "external child sent an unexpected reverse request; ignoring"
                        );
                    }
                }
                InboundFrame::Unparseable(line) => {
                    tracing::warn!(bytes = line.len(), "unparseable line from child");
                }
            }
        }
    }

    /// Answer one reverse permission ask from the child through the fence.
    /// A fence escape or a gate verdict answers immediately; a gate ask is
    /// delegated to the parent user through the bus, and the answer the user
    /// gives is returned to the child.
    async fn answer_child_permission(
        &mut self,
        req: &AcpRequest,
        fence: &PermissionFence,
    ) -> Result<(), AcpClientError> {
        let ask: RequestPermissionRequest = match req.params.as_ref() {
            Some(p) => serde_json::from_value(p.clone())
                .map_err(|e| AcpClientError::Protocol(protocol_err(format!("bad ask: {e}"))))?,
            None => {
                tracing::warn!("child permission ask carried no params; cancelling");
                return Ok(());
            }
        };
        let verdict =
            check_fence(&ask, &fence.workspace_root).or_else(|| check_gate(&fence.gate, &ask));
        let resp = match verdict {
            Some(v) => verdict_response(&ask, &v),
            None => self.delegate_to_parent(&ask, fence).await?,
        };
        let resp_id = match &req.id {
            AcpRequestId::Number(n) => *n,
            _ => return Ok(()),
        };
        let params = serde_json::to_value(&resp)
            .map_err(|e| AcpClientError::Protocol(protocol_err(format!("ask resp: {e}"))))?;
        let resp_frame = AcpResponse::ok(AcpRequestId::Number(resp_id), params);
        let frame = encode(&resp_frame)
            .map_err(|e| AcpClientError::Protocol(protocol_err(e.to_string())))?;
        self.pump.send_frame(frame).await?;
        Ok(())
    }

    /// Delegate one ask to the parent user through the agent bus and wait
    /// for the verdict or fail closed on timeout and disconnect.
    async fn delegate_to_parent(
        &mut self,
        ask: &RequestPermissionRequest,
        fence: &PermissionFence,
    ) -> Result<RequestPermissionResponse, AcpClientError> {
        let Some(bus) = fence.bus.clone() else {
            // No bus means no parent user to ask, so fail closed.
            return Ok(RequestPermissionResponse {
                outcome: RequestPermissionOutcome::Cancelled,
                meta: None,
            });
        };
        let call_id = ask.tool_call.tool_call_id.0.clone();
        let mut rx = bus.subscribe(&permission_response_topic(&fence.child_id, &call_id));
        bus.publish(
            permission_request_topic(),
            BusMessage::PermissionRequest {
                child_id: fence.child_id.clone(),
                subagent_type: fence.agent_type.clone(),
                call_id: call_id.clone(),
                tool: ask
                    .tool_call
                    .fields
                    .raw_input
                    .as_ref()
                    .and_then(|v| v.get("tool_name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_default(),
                input: ask
                    .tool_call
                    .fields
                    .raw_input
                    .clone()
                    .unwrap_or(Value::Null),
                reason: None,
            },
        );
        let resp = tokio::select! {
            r = rx.recv() => r,
            _ = sleep(PARENT_ASK_TIMEOUT) => {
                return Ok(RequestPermissionResponse {
                    outcome: RequestPermissionOutcome::Cancelled,
                    meta: None,
                });
            }
        };
        let Ok(BusMessage::PermissionResponse { approved, .. }) = resp else {
            return Ok(RequestPermissionResponse {
                outcome: RequestPermissionOutcome::Cancelled,
                meta: None,
            });
        };
        let kind = if approved {
            PermissionOptionKind::AllowOnce
        } else {
            PermissionOptionKind::RejectOnce
        };
        let option_id = ask
            .options
            .iter()
            .find(|o| o.kind == kind)
            .map(|o| o.option_id.clone());
        Ok(match option_id {
            Some(id) => RequestPermissionResponse {
                outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                    option_id: id,
                    meta: None,
                }),
                meta: None,
            },
            None => RequestPermissionResponse {
                outcome: RequestPermissionOutcome::Cancelled,
                meta: None,
            },
        })
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
        params: &Value,
    ) -> Result<(), ProtocolError> {
        let req = AcpRequest::new(id, method, params.clone());
        let frame = encode(&req)?;
        self.pump.send_frame(frame).await
    }

    async fn send_notification(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<(), ProtocolError> {
        let notif = AcpNotification::new(method, params.clone());
        let frame = encode(&notif)?;
        self.pump.send_frame(frame).await
    }

    async fn await_response(&mut self, id: i64) -> Result<Value, AcpClientError> {
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
