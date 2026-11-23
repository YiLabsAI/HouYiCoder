//! External ACP child execution. Resolves a definition whose backend names an
//! external ACP process, launches it through the shared launcher, and drives
//! one prompt to a terminal, producing the same spawn outcome an in-process
//! child produces.

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::time::sleep;

use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnOutcome};
use houyicoder_async::CancellationToken;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::SessionId;
use houyicoder_core::agent::multi_agent::bus_types::{
    BusMessage, ChildDescriptor, ChildRunMode, completed_topic, global_completed_topic,
};
use houyicoder_core::agent::multi_agent::registry::{
    AgentBackend, AgentDefinition, ExternalAcpSpec, IsolationMode,
};
use houyicoder_protocol::frontend::completion::CompletionStatus;

use super::MultiAgentRuntime;
use super::isolation_mode;
use crate::acp::client::session::{AcpChildSession, PermissionFence};

impl MultiAgentRuntime {
    /// The workspace root an external ACP child runs in: the runtime's cwd,
    /// which the composition root resolved from the session's workspace.
    pub(crate) fn external_workspace(&self) -> PathBuf {
        self.cwd.clone()
    }

    /// File a live external child's stop token, so the fleet's stop reaches
    /// the process the in-process registry cannot hold.
    pub(crate) fn register_external_child(&self, child_id: String, stop: CancellationToken) {
        self.external_children.register(child_id, stop);
    }

    /// Drop an external child's stop token once its process is gone.
    pub(crate) fn unregister_external_child(&self, child_id: &str) {
        self.external_children.unregister(child_id);
    }
}

/// The wall-clock ceiling an external child gets when the call names none. A
/// process child that stops making progress otherwise holds the parent's turn
/// with no way to reclaim it.
const DEFAULT_CHILD_CEILING: Duration = Duration::from_secs(3600);

/// Drive one external ACP child from a spawn request.
///
/// The child runs in its own worktree when the call or the definition asked
/// for one, otherwise in the parent's workspace. Its permission asks run
/// through the interception ladder under the session's own gate. Usage stays
/// unknown rather than zero, because an external child reports no token
/// figures the parent can sum honestly.
///
/// A background request is refused rather than run inline: the background
/// machinery keeps an in-process runner alive after the call returns, and a
/// process child has no such lifecycle here.
pub(super) async fn run_external_child(
    this: &MultiAgentRuntime,
    args: &SpawnArgs,
    definition: &AgentDefinition,
    cancel: CancellationToken,
) -> Result<SpawnOutcome, SpawnFailure> {
    let AgentBackend::ExternalAcp(spec) = &definition.backend else {
        return Err(SpawnFailure::ExternalFailed {
            reason: "the definition is not external".into(),
        });
    };
    if args.run_in_background {
        return Err(SpawnFailure::CapabilityDenied);
    }
    // A definition that asks for isolation is honoured even when the call does
    // not repeat it, because the definition is the configured default.
    let isolation = if definition.isolation == IsolationMode::Worktree {
        IsolationMode::Worktree
    } else {
        isolation_mode(&args.isolation)
    };
    let (workspace, worktree) = match isolation {
        IsolationMode::Worktree => {
            let Some(controller) = this.worktree_controller.as_ref() else {
                return Err(SpawnFailure::FenceFail);
            };
            let slug = format!("agent-{}", &SessionId::new().to_string()[..8]);
            match controller.enter_for_child(slug).await {
                Ok(cw) => (cw.worktree_path.clone(), Some(cw)),
                Err(error) => {
                    tracing::warn!(%error, "external child worktree setup failed");
                    return Err(SpawnFailure::FenceFail);
                }
            }
        }
        IsolationMode::None => (this.external_workspace(), None),
    };
    // Every exit below reclaims the tree; it is this call's to release
    // whatever the child did.
    let result = drive_in_workspace(this, args, spec, &workspace, cancel).await;
    if let (Some(cw), Some(controller)) = (worktree, this.worktree_controller.as_ref()) {
        controller.cleanup_child(cw).await;
    }
    result
}

/// The run itself, once the tree is chosen. Split out so the caller reclaims
/// the tree on every exit, including a failed connect or handshake.
async fn drive_in_workspace(
    this: &MultiAgentRuntime,
    args: &SpawnArgs,
    spec: &ExternalAcpSpec,
    workspace: &Path,
    cancel: CancellationToken,
) -> Result<SpawnOutcome, SpawnFailure> {
    let launcher = this.launcher.clone();
    let host_env: Vec<(String, String)> = env::vars().collect();
    let mut session =
        AcpChildSession::connect(&launcher, spec, workspace, &host_env).map_err(|e| {
            SpawnFailure::ExternalFailed {
                reason: e.to_string(),
            }
        })?;
    let child_sid =
        session
            .handshake(spec, workspace)
            .await
            .map_err(|e| SpawnFailure::ExternalFailed {
                reason: e.to_string(),
            })?;
    let child_string = child_sid.to_string();
    // The fence is built after the handshake, so its child id is the child's
    // own session id and two children of one type cannot answer each other's
    // asks.
    let fence = PermissionFence {
        workspace_root: workspace.to_path_buf(),
        gate: this.mode_gate.clone(),
        bus: this.bus.clone(),
        child_id: child_string.clone(),
        agent_type: args.subagent_type.clone(),
    };
    let descriptor = ChildDescriptor::new(
        child_string.clone(),
        args.subagent_type.clone(),
        ChildRunMode::Foreground,
    )
    .with_description(args.prompt_summary.clone())
    .with_backend(super::program_label(&spec.program));
    super::announce_spawn(this.bus.as_ref(), descriptor.clone());
    // The fleet's stop and the turn's stop both end the child; the ceiling
    // ends it too, so a wedged child cannot hold the turn forever.
    let stop = CancellationToken::new();
    let ceiling_hit = Arc::new(AtomicBool::new(false));
    this.register_external_child(child_string.clone(), stop.clone());
    let budget = args
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CHILD_CEILING);
    let watcher = {
        let stop = stop.clone();
        let cancel = cancel.clone();
        let ceiling_hit = Arc::clone(&ceiling_hit);
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => stop.cancel(),
                _ = sleep(budget) => {
                    ceiling_hit.store(true, Ordering::SeqCst);
                    stop.cancel();
                }
            }
        })
    };
    let prompt = args.prompt.clone();
    let store = this.store.clone();
    let outcome = session
        .drive_prompt(child_sid, &prompt, &store, &stop, Some(&fence))
        .await;
    watcher.abort();
    this.unregister_external_child(&child_string);
    session.shutdown().await;
    let ceiling = ceiling_hit.load(Ordering::SeqCst);
    let (status, ending, summary) = match &outcome {
        Ok(stop_reason) => (
            "completed",
            CompletionStatus::Completed,
            format!("external child ended with {stop_reason}"),
        ),
        Err(_) if ceiling => (
            "timed_out",
            CompletionStatus::TimedOut,
            ceiling_summary(budget),
        ),
        Err(error) => ("failed", CompletionStatus::Failed, error.to_string()),
    };
    if let Some(bus) = this.bus.as_ref() {
        let message = BusMessage::Completed {
            child: descriptor,
            status: ending,
            summary: summary.clone(),
        };
        bus.publish(&completed_topic(&child_string), message.clone());
        bus.publish(global_completed_topic(), message);
    }
    if ceiling {
        return Err(SpawnFailure::TimedOut {
            secs: budget.as_secs(),
        });
    }
    Ok(SpawnOutcome::foreground_without_usage(
        child_string,
        status,
        summary,
    ))
}

fn ceiling_summary(budget: Duration) -> String {
    format!(
        "external child was stopped at its {}s ceiling",
        budget.as_secs()
    )
}
