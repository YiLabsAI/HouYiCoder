//! The /mcp request handler: the status list, a reconnect, and an enable or
//! disable. Every reply carries a fresh status list, so the pane's rows and
//! the change that caused them land on the same reply.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::mcp::McpConfigScope;

use super::Server;
use super::frame_carrier::FrameCarrier;

/// Handle one /mcp request: list the servers, reconnect one, or toggle one.
pub(super) async fn handle(
    server: &mut Server,
    io: &mut FrameCarrier,
    req_id: RequestId,
    payload: FrontendRequest,
) -> Result<(), ProtocolError> {
    match payload {
        FrontendRequest::McpStatusList => reply_statuses(server, io, req_id).await,
        FrontendRequest::McpReconnect { name } => {
            if let Some(manager) = &server.mcp {
                manager.reconnect(&name).await;
            }
            reply_statuses(server, io, req_id).await
        }
        FrontendRequest::McpSetEnabled { name, enabled } => {
            toggle(server, io, req_id, name, enabled).await
        }
        _ => unreachable!("the caller matched the mcp variants"),
    }
}

/// Send the manager's current status list. A session with no manager has no
/// servers, which is the honest answer rather than an error.
async fn reply_statuses(
    server: &mut Server,
    io: &mut FrameCarrier,
    req_id: RequestId,
) -> Result<(), ProtocolError> {
    let statuses = match &server.mcp {
        Some(manager) => manager.statuses().await,
        None => Vec::new(),
    };
    server
        .send_response(io, req_id, ResponsePayload::McpStatus(statuses))
        .await
}

/// Flip one server's switch, persist a user-scope answer to the settings
/// file, and answer with the fresh list. The connection change is already
/// real when the settings write fails, so the failure reaches the user as
/// a notice rather than rolling back: the next session still reads the file,
/// and the user must know the two now disagree.
async fn toggle(
    server: &mut Server,
    io: &mut FrameCarrier,
    req_id: RequestId,
    name: String,
    enabled: bool,
) -> Result<(), ProtocolError> {
    let Some(manager) = &server.mcp else {
        return reply_statuses(server, io, req_id).await;
    };
    let user_scoped = manager
        .statuses()
        .await
        .iter()
        .any(|status| status.name == name && status.scope == McpConfigScope::User);
    if let Err(error) = manager.set_enabled(&name, enabled).await {
        return server
            .send_response(
                io,
                req_id,
                ResponsePayload::Error(ProtocolError::new(
                    ErrorCategory::InvalidRequest,
                    error,
                    false,
                )),
            )
            .await;
    }
    if user_scoped
        && let Some(path) = server.settings_path.as_deref()
        && let Err(error) = houyicoder_config::set_user_server_disabled(path, &name, !enabled)
    {
        server.notify_user(format!(
            "mcp: {name} is {state} for this session, but its settings entry \
             could not be updated: {error}",
            state = if enabled { "enabled" } else { "disabled" }
        ));
    }
    reply_statuses(server, io, req_id).await
}

#[cfg(test)]
mod tests;
