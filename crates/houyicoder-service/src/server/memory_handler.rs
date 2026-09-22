//! Handlers for memory requests in the server dispatch.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::memory::MemoryToggleWhich;

use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter as pa;

impl Server {
    pub(super) async fn handle_memory_list(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let entries = pa::memory_view::map_memory_list(self.runner.memory_list());
        self.send_response(io, req_id, ResponsePayload::MemoryList(entries))
            .await
    }

    pub(super) async fn handle_memory_show(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        key: String,
    ) -> Result<(), ProtocolError> {
        let entry = pa::memory_view::map_memory_entry(self.runner.memory_show(&key));
        self.send_response(io, req_id, ResponsePayload::MemoryShow(entry))
            .await
    }

    pub(super) async fn handle_memory_forget(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        key: String,
        scope: String,
    ) -> Result<(), ProtocolError> {
        match self.runner.memory_forget(&key, &scope) {
            Ok(()) | Err(houyicoder_context::MemoryError::NotFound) => {
                let entries = pa::memory_view::map_memory_list(self.runner.memory_list());
                self.send_response(io, req_id, ResponsePayload::MemoryList(entries))
                    .await
            }
            Err(e) => {
                self.send_response(
                    io,
                    req_id,
                    ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::Internal,
                        format!("memory forget failed: {e}"),
                        false,
                    )),
                )
                .await
            }
        }
    }

    pub(super) async fn handle_memory_toggle_state(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let state = self.runner.memory_gate_state();
        let toggle = pa::memory_view::map_toggle_state(state.auto_memory, state.auto_dream);
        self.send_response(io, req_id, ResponsePayload::ToggleState(toggle))
            .await
    }

    pub(super) async fn handle_memory_toggle(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        which: MemoryToggleWhich,
    ) -> Result<(), ProtocolError> {
        let current = self.runner.memory_gate_state();
        let (auto_memory, auto_dream) = match which {
            MemoryToggleWhich::Auto => (!current.auto_memory, current.auto_dream),
            MemoryToggleWhich::Dream => (current.auto_memory, !current.auto_dream),
        };
        if let Err(e) = houyicoder_config::save_toggles_to(
            &self.settings_path,
            &houyicoder_config::MemoryToggles {
                auto_memory,
                auto_dream,
            },
        ) {
            return self
                .send_response(
                    io,
                    req_id,
                    ResponsePayload::Error(ProtocolError::new(
                        ErrorCategory::Internal,
                        format!("failed to save settings: {e}"),
                        false,
                    )),
                )
                .await;
        }
        match which {
            MemoryToggleWhich::Auto => self.runner.set_auto_memory(auto_memory),
            MemoryToggleWhich::Dream => self.runner.set_auto_dream(auto_dream),
        }
        let toggle = pa::memory_view::map_toggle_state(auto_memory, auto_dream);
        self.send_response(io, req_id, ResponsePayload::ToggleState(toggle))
            .await
    }
}
