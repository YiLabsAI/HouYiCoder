//! Reconnection contracts for interrupted sessions.
//!
//! These tests verify that a new connection restores pending permission work,
//! queued input mapping, event order, and lifecycle ownership without
//! reviving terminal sessions or accepting unrelated responses. Disconnects
//! at each recovery boundary must remain resumable and preserve prior choices.

use super::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponseEnvelope, ClientResponsePayload, EventEnvelope, RequestId,
    ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::error::ErrorCategory;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, ContentBlock};
use houyicoder_protocol::frontend::session_update::SessionUpdate;
use houyicoder_protocol::frontend::trust::TrustAccept;
use houyicoder_protocol::frontend::{FrontendEvent, FrontendRequest, QueuedInput};
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_session::SessionStore;
use serde_json::Value;
use tokio::sync::{Notify, oneshot};

use crate::composition::SessionHost;
use crate::lifecycle::{Lifecycle, LifecycleState, SessionLeaseStore};
use houyicoder_context::PermissionVerdict;
use houyicoder_context::SessionEvent;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::framing::encode;
use tokio::time::timeout;

mod harness;
mod lease;
mod queue_replay;
mod resume;
mod verdict_errors;
