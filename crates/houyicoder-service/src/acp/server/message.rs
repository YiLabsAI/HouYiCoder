//! Free helpers the serve loop and its turn mechanics share, split out so the
//! server module stays under the size gate.

use std::future::pending;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use houyicoder_context::SessionId;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::error::ErrorCategory;
use houyicoder_protocol::error::ProtocolError;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::SessionUpdate;

use crate::turn_events::{TurnEventSubscriber, TurnStreamEvent};

/// The plain text of a prompt, or an error message naming the first non-text
/// block the agent does not accept. The agent declared promptCapabilities
/// false for image, audio, and embedded context at initialize, so a block of
/// any of those kinds is rejected here rather than silently dropped.
pub(super) fn prompt_text(prompt: Vec<ContentBlock>) -> Result<String, String> {
    let mut text = String::new();
    for block in prompt {
        match block {
            ContentBlock::Text { text: t } => text.push_str(&t),
            ContentBlock::Image { .. } => {
                return Err(
                    "image content is not supported (agent declared promptCapabilities.image false)"
                        .into(),
                );
            }
            ContentBlock::ResourceLink { uri, .. } => {
                return Err(format!(
                    "resource_link attachments are not supported: {uri}"
                ));
            }
            _ => return Err("unsupported content block".into()),
        }
    }
    Ok(text)
}

/// The next live event, or pending forever when no stream is installed. A
/// select branch that never resolves keeps the run future and the frame
/// reader as the only live branches, so the absence of a stream costs
/// nothing. A stream whose producer is gone is dropped here rather than
/// polling a closed receiver on every turn of the loop.
pub(super) async fn next_turn_event(events: &mut Option<TurnEventSubscriber>) -> TurnStreamEvent {
    if let Some(stream) = events.as_mut()
        && let Some(event) = stream.recv().await
    {
        return event;
    }
    *events = None;
    pending().await
}

/// A session/update notification in the envelope the base protocol defines:
/// the session it belongs to, and the update nested under its own key. A
/// client that reads the envelope drops an update sent without it.
pub(super) fn session_update(
    session: SessionId,
    update: &SessionUpdate,
) -> Result<AcpNotification, ProtocolError> {
    let params = serde_json::json!({
        "sessionId": session.to_string(),
        "update": serde_json::to_value(update).map_err(|e| {
            ProtocolError::new(ErrorCategory::InvalidFrame, e.to_string(), false)
        })?,
    });
    Ok(AcpNotification::new("session/update", params))
}

/// The current wall clock as milliseconds since the Unix epoch.
pub(super) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
