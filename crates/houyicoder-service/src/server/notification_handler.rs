//! Handler for fire-and-forget session notifications from the client.
//!
//! Client notifications steer, queue, or abort work without expecting a
//! response frame: aborting the active run, enqueueing or removing a mid-turn
//! user prompt, steering a running child agent, cancelling a child's current
//! turn, or terminating background children.

use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::frontend::QueuedInput;

use super::Server;

impl Server {
    /// Route a session notification received during or between runs.
    ///
    /// session/cancel aborts the active run; session/inject enqueues a user
    /// message for the next turn boundary; session/queue_remove drops the
    /// identified queued item; session/inject_child, session/cancel_child_turn,
    /// session/kill_child, and session/kill_all control delegated child runs.
    pub(super) fn handle_session_notification(&self, notif: &AcpNotification) {
        match notif.method.as_str() {
            "session/cancel" => self.runner.abort(),
            "session/inject" => {
                let input = notif.params.as_ref().and_then(|params| {
                    params
                        .get("input")
                        .and_then(|value| serde_json::from_value(value.clone()).ok())
                        .or_else(|| {
                            params
                                .get("text")
                                .and_then(|value| value.as_str())
                                .filter(|text| !text.is_empty())
                                .map(QueuedInput::new)
                        })
                });
                if let Some(input) = input {
                    self.runner.enqueue_input(input);
                }
            }
            "session/queue_remove" => {
                let Some(params) = notif.params.as_ref() else {
                    return;
                };
                if let Some(id) = params
                    .get("id")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                {
                    self.runner.remove_input(id);
                } else if let Some(text) = params.get("text").and_then(|value| value.as_str()) {
                    self.runner.remove_input_by_text(text);
                }
            }
            "session/inject_child" => {
                let Some(child_sid) = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("childSid"))
                    .and_then(|v| v.as_str())
                else {
                    return;
                };
                let text = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if text.is_empty() {
                    return;
                }
                if let Err(e) = self.runner.steer_child(child_sid, text) {
                    tracing::warn!("inject_child: {e}");
                }
            }
            "session/cancel_child_turn" => {
                let Some(child_sid) = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("childSid"))
                    .and_then(|v| v.as_str())
                else {
                    return;
                };
                self.runner.cancel_child(child_sid);
            }
            "session/kill_child" => {
                let Some(child_sid) = notif
                    .params
                    .as_ref()
                    .and_then(|p| p.get("childSid"))
                    .and_then(|v| v.as_str())
                else {
                    return;
                };
                self.runner.kill_child(child_sid);
            }
            "session/kill_all" => {
                self.runner.kill_all_children();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
