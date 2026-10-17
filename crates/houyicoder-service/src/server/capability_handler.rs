//! Capability listing handlers: registered tools and the delegatable agent
//! types.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::ProtocolError;

use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter::agents::agent_entries;
use houyicoder_protocol::frontend::tools::ToolEntry;

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
            .map(|(name, description)| ToolEntry { name, description })
            .collect::<Vec<_>>();
        self.send_response(io, req_id, ResponsePayload::Tools(tools))
            .await
    }

    /// Reply with the sub-agent types the model may delegate to. The same
    /// definitions produce the prompt paragraph, so the pane and the model
    /// name one type set.
    pub(super) async fn handle_agents(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let entries = agent_entries(self.runner.agent_types());
        self.send_response(io, req_id, ResponsePayload::Agents(entries))
            .await
    }
}
