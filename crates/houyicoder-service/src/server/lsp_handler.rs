//! The /lsp request handler: the status list, a restart, and an enable or
//! disable. Every successful reply carries a fresh status list, so the
//! pane's rows and the change that caused them land on the same reply.

use std::sync::Arc;

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::lsp::LspConfigScope;

use super::Server;
use super::frame_carrier::FrameCarrier;

/// Handle one /lsp request: list the servers, restart one, or toggle one.
pub(super) async fn handle(
    server: &mut Server,
    io: &mut FrameCarrier,
    req_id: RequestId,
    payload: FrontendRequest,
) -> Result<(), ProtocolError> {
    match payload {
        FrontendRequest::LspStatusList => reply_statuses(server, io, req_id).await,
        FrontendRequest::LspRestart { name } => {
            // The stop and the handshake can take seconds, and this reply
            // shares its loop with model-stream flushing; launching in the
            // background keeps a wedged server from freezing the session.
            // The pane polls a restarting row until it settles, and that
            // poll is what reports a launch that failed.
            if let Some(manager) = &server.lsp {
                let manager = Arc::clone(manager);
                tokio::spawn(async move {
                    if let Err(error) = manager.restart(&name).await {
                        tracing::warn!(
                            server = %name,
                            %error,
                            "the requested language server restart failed"
                        );
                    }
                });
            }
            reply_statuses(server, io, req_id).await
        }
        FrontendRequest::LspSetEnabled { name, enabled } => {
            toggle(server, io, req_id, name, enabled).await
        }
        _ => unreachable!("the caller matched the lsp variants"),
    }
}

/// Send the manager's current status list. A session with no manager has no
/// servers, which is the honest answer rather than an error.
async fn reply_statuses(
    server: &mut Server,
    io: &mut FrameCarrier,
    req_id: RequestId,
) -> Result<(), ProtocolError> {
    let statuses = match &server.lsp {
        Some(manager) => manager.statuses().await,
        None => Vec::new(),
    };
    server
        .send_response(io, req_id, ResponsePayload::LspStatus(statuses))
        .await
}

/// Answer one refusal as the protocol error that carries its reason, so the
/// pane shows why nothing changed.
async fn reply_error(
    server: &mut Server,
    io: &mut FrameCarrier,
    req_id: RequestId,
    message: String,
) -> Result<(), ProtocolError> {
    server
        .send_response(
            io,
            req_id,
            ResponsePayload::Error(ProtocolError::new(
                ErrorCategory::InvalidRequest,
                message,
                false,
            )),
        )
        .await
}

/// Flip one server's switch, persist a user-scope answer to the settings
/// file, and answer with the fresh list. The connection change is already
/// real when the settings write fails, so the failure reaches the user as a
/// notice rather than rolling back: the next session still reads the file,
/// and the user must know the two now disagree.
async fn toggle(
    server: &mut Server,
    io: &mut FrameCarrier,
    req_id: RequestId,
    name: String,
    enabled: bool,
) -> Result<(), ProtocolError> {
    let Some(manager) = &server.lsp else {
        return reply_statuses(server, io, req_id).await;
    };
    let user_scoped = manager
        .statuses()
        .await
        .iter()
        .any(|status| status.name == name && status.scope == LspConfigScope::User);
    if let Err(error) = manager.set_enabled(&name, enabled).await {
        return reply_error(server, io, req_id, error).await;
    }
    if user_scoped
        && let Some(path) = server.settings_path.as_deref()
        && let Err(error) = houyicoder_config::set_user_lsp_disabled(path, &name, !enabled)
    {
        server.notify_user(format!(
            "lsp: {name} is {state} for this session, but its settings entry \
             could not be updated: {error}",
            state = if enabled { "enabled" } else { "disabled" }
        ));
    }
    reply_statuses(server, io, req_id).await
}

#[cfg(test)]
mod tests;
