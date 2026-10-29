//! Handlers for the sandbox pane: the boundaries a session runs inside, the
//! writes recovery can still undo, and the restore a confirmed row
//! authorizes.
//!
//! Neither answer costs a model turn. The listing judges every recorded write
//! against what its target holds now, and the restore re-reads the target under
//! the same lock the write path takes, so a write this process ran in between
//! is not undone by mistake.

use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};

use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter::checkpoint_view;

impl Server {
    /// Answer the boundaries the session runs inside.
    pub(super) async fn handle_sandbox_boundaries(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let Some(sandbox) = self.sandbox_session.clone() else {
            return self.refuse_sandbox(io, req_id, NO_SANDBOX).await;
        };
        let view = checkpoint_view::map_boundaries(sandbox.as_ref());
        self.send_response(io, req_id, ResponsePayload::SandboxBoundaries(view))
            .await
    }

    /// Answer every write the session recorded, with what each target holds
    /// now.
    pub(super) async fn handle_sandbox_checkpoints(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let Some(journal) = self.file_transaction.clone() else {
            return self.refuse_sandbox(io, req_id, NO_JOURNAL).await;
        };
        let Some(sandbox) = self.sandbox_session.clone() else {
            return self.refuse_sandbox(io, req_id, NO_SANDBOX).await;
        };
        match journal.recover(self.session, sandbox.as_ref()).await {
            Ok(report) => {
                let view = checkpoint_view::map_checkpoints(report);
                self.send_response(io, req_id, ResponsePayload::SandboxCheckpoints(view))
                    .await
            }
            Err(error) => self.fail_sandbox(io, req_id, &error.to_string()).await,
        }
    }

    /// Answer both sides of one recorded write, as text, for its detail. Only
    /// that row's stored images are opened, so the listing stays cheap however
    /// many writes the session recorded.
    pub(super) async fn handle_sandbox_texts(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        operation: String,
    ) -> Result<(), ProtocolError> {
        let Some(journal) = self.file_transaction.clone() else {
            return self.refuse_sandbox(io, req_id, NO_JOURNAL).await;
        };
        match journal.text(self.session, &operation).await {
            Ok(write) => {
                let view = checkpoint_view::map_texts(write);
                self.send_response(io, req_id, ResponsePayload::SandboxTexts(view))
                    .await
            }
            Err(error) => self.fail_sandbox(io, req_id, &error.to_string()).await,
        }
    }

    /// Put one recorded write's earlier bytes back.
    pub(super) async fn handle_sandbox_restore(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        operation: String,
    ) -> Result<(), ProtocolError> {
        let Some(journal) = self.file_transaction.clone() else {
            return self.refuse_sandbox(io, req_id, NO_JOURNAL).await;
        };
        let Some(sandbox) = self.sandbox_session.clone() else {
            return self.refuse_sandbox(io, req_id, NO_SANDBOX).await;
        };
        match journal
            .restore(self.session, sandbox.as_ref(), &operation)
            .await
        {
            Ok(outcome) => {
                let view = checkpoint_view::map_restore(operation, outcome);
                self.send_response(io, req_id, ResponsePayload::SandboxRestored(view))
                    .await
            }
            Err(error) => self.fail_sandbox(io, req_id, &error.to_string()).await,
        }
    }

    /// Answer a sandbox request this session cannot serve, naming what is
    /// missing.
    async fn refuse_sandbox(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        message: &str,
    ) -> Result<(), ProtocolError> {
        self.send_response(
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

    /// Answer a sandbox request the host tried and could not complete. The
    /// message is the failure the recovery reported, so the user sees the
    /// cause rather than a bare refusal.
    async fn fail_sandbox(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        message: &str,
    ) -> Result<(), ProtocolError> {
        self.send_response(
            io,
            req_id,
            ResponsePayload::Error(ProtocolError::new(ErrorCategory::Internal, message, false)),
        )
        .await
    }
}

/// The answer when the session carries no sandbox: there is no fence to
/// describe.
const NO_SANDBOX: &str =
    "this session runs without a sandbox session, so it has no boundaries to report";

/// The answer when the session's store keeps no root on disk: no write record
/// outlives the process, so there is nothing to list or restore.
const NO_JOURNAL: &str = "this session keeps no write journal: its store holds no root on disk";

#[cfg(test)]
mod tests;
