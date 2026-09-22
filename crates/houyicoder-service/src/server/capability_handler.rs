//! Capability listing handlers: registered tools and the agent directory.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::ProtocolError;

use super::{Server, frame_carrier::FrameCarrier};

impl Server {
    /// Reply with the registered tools so /tools can show what the agent can
    /// do without the host reading source.
    pub(super) async fn handle_tool_list(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let tools = self
            .runner
            .tools_snapshot()
            .into_iter()
            .map(
                |(name, description)| houyicoder_protocol::frontend::tools::ToolEntry {
                    name,
                    description,
                },
            )
            .collect::<Vec<_>>();
        self.send_response(io, req_id, ResponsePayload::Tools(tools))
            .await
    }

    /// Reply with the agent directory. The directory doubles as the model's
    /// prompt paragraph and is byte-stable for the prompt cache.
    pub(super) async fn handle_agents(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let dir = self.runner.agent_directory().unwrap_or_default();
        self.send_response(io, req_id, ResponsePayload::Agents(dir))
            .await
    }
}
