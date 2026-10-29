//! Handlers for the /worktrees pane verbs: the preview a removal is decided
//! from, the enter that moves the session into a worktree, and the removal a
//! confirmed preview authorizes.
//!
//! None of them costs a model turn. Each names a directory, and the host
//! answers whether it is a worktree this product created before acting. The
//! work reads or takes a tree apart, so it runs on the blocking pool rather
//! than occupying the worker that serves the event stream.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_core::agent::worktree::controller::WorktreeController;
use houyicoder_core::agent::worktree::session::WorktreeError;
use houyicoder_protocol::envelope::{RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::worktree::WorktreeRemoveOutcome;
use tokio::task::spawn_blocking;

use super::{Server, frame_carrier::FrameCarrier};
use crate::protocol_adapter::worktree_view;

impl Server {
    /// Answer what removing a worktree would discard. Read-only: no lease is
    /// taken and nothing is written.
    pub(super) async fn handle_worktree_preview(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        path: String,
    ) -> Result<(), ProtocolError> {
        let Some(controller) = self.runner.worktree_controller() else {
            return self.refuse_worktree(io, req_id, NO_REPOSITORY).await;
        };
        let preview = spawn_blocking(move || {
            let (target, preview) = controller.preview_at(Path::new(&path))?;
            Ok::<_, WorktreeError>((target.name(), preview))
        })
        .await;
        match preview {
            Ok(Ok((name, preview))) => {
                let view = worktree_view::map_preview(&name, preview);
                self.send_response(io, req_id, ResponsePayload::WorktreePreview(view))
                    .await
            }
            Ok(Err(error)) => self.refuse_worktree(io, req_id, &error.to_string()).await,
            Err(error) => {
                self.refuse_worktree(io, req_id, &format!("the preview did not finish: {error}"))
                    .await
            }
        }
    }

    /// Move the session into a worktree. The controller refuses a second enter,
    /// a directory this product did not create, and an enter while a
    /// sandboxed command is still running.
    pub(super) async fn handle_worktree_enter(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        path: String,
    ) -> Result<(), ProtocolError> {
        let Some(controller) = self.runner.worktree_controller() else {
            return self.refuse_worktree(io, req_id, NO_REPOSITORY).await;
        };
        let worker: Arc<WorktreeController> = Arc::clone(&controller);
        let prepared = spawn_blocking(move || worker.enter_at_prepared(Path::new(&path))).await;
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => return self.refuse_worktree(io, req_id, &error.to_string()).await,
            Err(error) => {
                return self
                    .refuse_worktree(io, req_id, &format!("the enter did not finish: {error}"))
                    .await;
            }
        };
        match controller.finish_enter(prepared).await {
            Ok(entered) => {
                let view =
                    worktree_view::map_entered(entered.worktree_path, entered.worktree_branch);
                self.send_response(io, req_id, ResponsePayload::WorktreeEntered(view))
                    .await
            }
            Err(error) => self.refuse_worktree(io, req_id, &error.to_string()).await,
        }
    }

    /// Remove a worktree the user confirmed from its preview. The controller
    /// re-reads the tree under the lease it takes and removes nothing unless
    /// the token still names what it finds.
    pub(super) async fn handle_worktree_remove(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        path: String,
        token: String,
        delete_unmerged_branch: bool,
    ) -> Result<(), ProtocolError> {
        let Some(controller) = self.runner.worktree_controller() else {
            return self.refuse_worktree(io, req_id, NO_REPOSITORY).await;
        };
        let tree = PathBuf::from(&path);
        let worker: Arc<WorktreeController> = Arc::clone(&controller);
        let removal = spawn_blocking(move || {
            worker.remove_at_previewed(Path::new(&path), &token, delete_unmerged_branch)
        })
        .await;
        match removal {
            Ok(result) => {
                let outcome = worktree_view::map_removal(result);
                if matches!(outcome, Ok(WorktreeRemoveOutcome::Removed)) {
                    controller.fire_removed(tree).await;
                }
                match outcome {
                    Ok(outcome) => {
                        self.send_response(io, req_id, ResponsePayload::WorktreeRemoved(outcome))
                            .await
                    }
                    Err(error) => self.refuse_worktree(io, req_id, &error.to_string()).await,
                }
            }
            Err(error) => {
                self.refuse_worktree(io, req_id, &format!("the removal did not finish: {error}"))
                    .await
            }
        }
    }

    /// Answer a worktree request that the host cannot carry out, naming the
    /// reason the user can act on.
    async fn refuse_worktree(
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
}

/// The answer when the session has nothing to manage worktrees with: it runs
/// outside a git repository, or without the isolation that fences one.
const NO_REPOSITORY: &str = "this session manages no worktrees: it runs outside a git repository, or without a sandbox session";
