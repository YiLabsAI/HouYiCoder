//! Reading the permission response frame: dispatch notifications, defer
//! crossing client requests, and report a cancel or a closed connection.

use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{ClientFrame, ClientResponseEnvelope, RequestId};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};

use super::super::{Server, frame_carrier::FrameCarrier};

impl Server {
    /// Read frames until the permission response for ask_id arrives. A
    /// session/cancel notification returns None so the caller answers the turn
    /// as denied; a closed connection is an error.
    pub(super) async fn read_permission_response(
        &mut self,
        io: &mut FrameCarrier,
        ask_id: RequestId,
    ) -> Result<Option<ClientResponseEnvelope>, ProtocolError> {
        loop {
            let frame = match io.next_live_frame().await? {
                Some(f) => f,
                None => {
                    return Err(ProtocolError::new(
                        ErrorCategory::Unavailable,
                        "client closed mid-permission",
                        false,
                    ));
                }
            };
            if let Ok(notif) = serde_json::from_str::<AcpNotification>(&frame) {
                let is_cancel = notif.method == "session/cancel";
                self.handle_session_notification(&notif);
                if is_cancel {
                    return Ok(None);
                }
                continue;
            }
            if let Ok(ClientFrame::Response(r)) = serde_json::from_str::<ClientFrame>(&frame)
                && r.req_id == ask_id
            {
                return Ok(Some(r));
            }
            if matches!(
                serde_json::from_str::<ClientFrame>(&frame),
                Ok(ClientFrame::Request(_))
            ) && io.defer_frame(frame.clone())
            {
                continue;
            }
            tracing::warn!(
                "ask-wait: dropped a non-matching frame while waiting for the permission response (first 80 chars): {}",
                frame.chars().take(80).collect::<String>()
            );
        }
    }
}
