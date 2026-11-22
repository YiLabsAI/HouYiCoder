//! Foreground wait and background wall-clock budget calculation for shell calls.

use std::future::pending;
use std::time::Duration;

use houyicoder_api::background::{CommandCompletion, DetachedCommand};
use houyicoder_async::CancellationToken;
use houyicoder_context::ExecConfig;
use houyicoder_protocol::extension::ToolError;
use serde_json::Value;
use tokio::time::sleep_until;

use super::output::background_output;
use super::{BackgroundPolicy, MAX_TIMEOUT_MS};

/// Extended wall-clock ceiling applied to commands eligible to run in the
/// background registry so multi-minute builds and remote pushes are not cut
/// short by the two-minute foreground default while still preventing runaway
/// processes from living forever.
pub(super) const BACKGROUND_WALL_TIMEOUT_MS: u64 = 3_600_000;

/// Resolved timing parameters for a single shell invocation.
pub(super) struct RunBudgets {
    /// Whether the caller asked to detach immediately without waiting.
    pub(super) explicit_background: bool,
    /// Wall-clock limit handed to the sandbox process execution.
    pub(super) exec_wall_ms: u64,
    /// Foreground wait before automatic handover, when enabled.
    pub(super) handover_budget: Option<Duration>,
    /// Foreground ceiling enforced when all background slots are occupied.
    pub(super) fallback_foreground_ms: u64,
}

pub(super) fn resolve_run_budgets(
    input: &Value,
    in_child: bool,
    policy_budget: Option<Duration>,
) -> RunBudgets {
    let declared_ms = declared_timeout_ms(input);
    let fallback_foreground_ms = wall_budget_ms(input);

    let Some(budget) = policy_budget.filter(|_| !in_child) else {
        return RunBudgets {
            explicit_background: false,
            exec_wall_ms: fallback_foreground_ms,
            handover_budget: None,
            fallback_foreground_ms,
        };
    };

    let explicit_background = input
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if explicit_background {
        return RunBudgets {
            explicit_background: true,
            exec_wall_ms: BACKGROUND_WALL_TIMEOUT_MS,
            handover_budget: Some(Duration::ZERO),
            fallback_foreground_ms,
        };
    }

    if let Some(short_ms) = declared_ms
        && Duration::from_millis(short_ms) <= budget
    {
        return RunBudgets {
            explicit_background: false,
            exec_wall_ms: short_ms,
            handover_budget: None,
            fallback_foreground_ms: short_ms,
        };
    }

    RunBudgets {
        explicit_background: false,
        exec_wall_ms: BACKGROUND_WALL_TIMEOUT_MS,
        handover_budget: Some(budget),
        fallback_foreground_ms,
    }
}

pub(super) fn wall_budget_ms(input: &Value) -> u64 {
    declared_timeout_ms(input).unwrap_or_else(|| ExecConfig::default().wall_timeout_ms)
}

fn declared_timeout_ms(input: &Value) -> Option<u64> {
    ["timeout_ms", "timeout"]
        .iter()
        .find_map(|name| input.get(*name).and_then(Value::as_u64))
        .map(|ms| ms.clamp(1, MAX_TIMEOUT_MS))
}

/// Outcome of attempting to move a running command into the background
/// registry when its foreground wait budget expires.
pub(super) enum HandoverOutcome {
    /// Ownership transferred to the background registry.
    Backgrounded(Value),
    /// Slots were occupied so the call waited out the command in foreground.
    Completed(CommandCompletion),
    /// Cancelled while waiting in foreground fallback.
    Interrupted,
}

pub(super) async fn handover_or_wait(
    detached: DetachedCommand,
    policy: &BackgroundPolicy,
    budgets: &RunBudgets,
    cancel: Option<&CancellationToken>,
) -> Result<HandoverOutcome, ToolError> {
    let tail_lines = detached.tail.snapshot();
    match policy
        .launcher
        .try_launch(detached, budgets.explicit_background)
    {
        Ok(launched) => {
            let waited = (!budgets.explicit_background).then_some(policy.budget);
            Ok(HandoverOutcome::Backgrounded(background_output(
                launched.id,
                tail_lines,
                waited,
                launched.duplicate,
            )))
        }
        Err((_returned, refusal)) if budgets.explicit_background => {
            Err(ToolError::Failed(format!("bash: {refusal}")))
        }
        Err((returned, _full)) => {
            match finish_foreground_wait(returned, cancel, budgets.fallback_foreground_ms).await {
                Some(done) => Ok(HandoverOutcome::Completed(done)),
                None => Ok(HandoverOutcome::Interrupted),
            }
        }
    }
}

/// Finish awaiting a command in the foreground after the background registry
/// declined handover because all active slots were occupied.
async fn finish_foreground_wait(
    mut detached: DetachedCommand,
    cancel: Option<&CancellationToken>,
    fallback_foreground_ms: u64,
) -> Option<CommandCompletion> {
    let deadline = detached.started_at + Duration::from_millis(fallback_foreground_ms);
    tokio::select! {
        biased;
        completion = &mut detached.completion => Some(completion),
        _ = async {
            match cancel {
                Some(token) => token.cancelled().await,
                None => pending().await,
            }
        } => None,
        _ = sleep_until(deadline.into()) => {
            let tail_lines = detached.tail.snapshot();
            drop(detached);
            Some(CommandCompletion {
                exit_code: None,
                stdout: tail_lines.join("\n"),
                stderr: format!(
                    "bash: stopped, wall-clock {}s exceeded while background slots were full",
                    fallback_foreground_ms.div_ceil(1000)
                ),
                timed_out: true,
                error: None,
                capture: None,
            })
        }
    }
}
