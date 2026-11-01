//! The command execution pipeline: wrap the command for the sandboxed shell,
//! start it under the fence, drain its output under the wall timeout, and reap
//! the process group on every exit path.

use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use houyicoder_api::capture::RawOutputReceiver;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_resilience::resource_breaker::SpawnEvent;
use tokio::process::Command;
use tokio::time::timeout;

use crate::output_capture::{CapturedOutput, DRAIN_GRACE, capture, pipe_streams};
use crate::timeout;

use super::MacSeatbeltSession;
use super::command::annotate_fence_refusal;
use super::process::{BreakerSpanGuard, ExecCountGuard, TreeKillGuard, tree_cpu_secs};

impl MacSeatbeltSession {
    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    pub(super) async fn exec_inner(
        &self,
        command: String,
        config: ExecConfig,
        lines: Option<Arc<AtomicI64>>,
        receiver: Option<Arc<dyn RawOutputReceiver>>,
    ) -> Result<ExecResult, SandboxError> {
        let profile = self.current_profile();
        let cwd = self.effective_workspace();
        // Wrap the command to source the login-shell env snapshot first, so
        // the non-login shell still sees the user's PATH / aliases /
        // functions. No-op when the snapshot is unavailable (degraded). The
        // snapshot file lives under the config home, which the profile
        // allow-backs so the sandboxed shell can read it.
        let command = self
            .shell_snapshot
            .lock()
            .expect("shell_snapshot mutex")
            .inject(&command, Some(&self.tmpdir.to_string_lossy()));
        let breaker = self.breaker.clone();
        // A worktree session used to run with HOME rewritten to the worktree
        // plus the system git config switched off, so git would not touch two
        // paths the profile denied. Both denials are gone: the etc allow-back
        // now reaches the kernel, and the home git config is readable with the
        // write still denied. The environment is therefore left alone, which
        // matters because rewriting HOME also hid the user's real git identity
        // and aliases from every command in the session.
        let exec_count = Arc::clone(&self.exec_count);
        exec_count.fetch_add(1, Ordering::SeqCst);
        // RAII guard: decrement the in-flight exec count on EVERY exit
        // path (Ok, Err, wall-timeout, future-drop/cancel) so a worktree
        // enter's active_exec_count check is not fooled by a leaked count.
        let _exec_guard = ExecCountGuard(Arc::clone(&exec_count));
        // Aggregate breaker: refuse the spawn up front while Open (the
        // cool-down after an aggregate overrun). Fail-closed toward
        // refusal — a breaker that has tripped must not silently let a
        // new spawn through. No pgid/guard yet; we never spawned.
        if let Some(b) = &breaker
            && let Err(e) = b.try_acquire()
        {
            return Err(SandboxError::BreakerOpen(e.to_string()));
        }
        // A session may run unfenced (via the --no-sandbox flag or the
        // HOUYICODER_SANDBOX_NO_ENFORCE test escape hatch). The kernel refuses
        // to apply a second seatbelt inside an existing one, so a suite running
        // under a session's own fence cannot spawn sandbox-exec at all.
        let unfenced = crate::is_unfenced();
        let mut cmd = if unfenced {
            Command::new(&self.shell)
        } else {
            Command::new("sandbox-exec")
        };
        // Export the per-session temp dir as TMPDIR so heredoc temp files and
        // tools that honor TMPDIR land inside the fence (the profile
        // allow-lists this exact path). TMPPREFIX covers a host that fell back
        // to zsh, whose heredoc temp ignores TMPDIR; bash ignores it. Set
        // explicitly rather than inheriting the parent TMPDIR, which points
        // outside the fence.
        let tmpdir_str = self.tmpdir.to_string_lossy().into_owned();
        cmd.env("TMPDIR", &tmpdir_str)
            .env("TMPPREFIX", format!("{tmpdir_str}/zsh"));
        if !unfenced {
            // The profile and its separator belong to the fenced form only.
            // The program under it is sandbox-exec, so the shell is an
            // argument. Unfenced there is no wrapper, and the shell IS the
            // program, so passing it again would ask bash to execute itself.
            cmd.arg("-p").arg(&profile).arg("--").arg(&self.shell);
        }
        cmd.arg("-c").arg(&command).current_dir(&cwd);
        pipe_streams(&mut cmd);
        cmd.kill_on_drop(true);
        // process_group(0) puts child + all descendants in a NEW process
        // group (pgid = child pid) so killpg(pgid) reaps the whole tree on
        // timeout — the direct-child-only kill leaves orphan
        // fix. tokio's process_group is a safe wrapper (no pre_exec, which
        // std marks unsafe and the workspace denies).
        cmd.process_group(0);
        // The cpu, address space and process count budgets are not applied
        // on macOS: there is no safe in-child setrlimit (pre_exec is
        // unsafe-blocked), so the resource fence here is the wall timeout
        // plus the group kill above. Linux defers them for a different
        // reason: the rlimit primitives are per user, not per tree.
        let _ = (config.cpu_secs, config.as_bytes, config.nproc);
        let child = cmd
            .spawn()
            .map_err(|e| SandboxError::SandboxUnavailable(format!("sandbox-exec spawn: {e}")))?;
        // After process_group(0) the child's pgid equals its pid.
        let pgid = child.id().unwrap_or(0) as i32;
        // RAII tree-kill guard: on Drop (every exit path — Ok, Err,
        // wall-timeout, AND future-drop/Ctrl-C) killpg the whole group.
        // Without this, dropping the exec future (cancel) would only
        // kill_on_drop the direct child, leaving grandchildren alive —
        // re-introducing the orphan-process problem.
        let _tree_guard = TreeKillGuard::new(pgid);
        // Count one in-flight proc + arm an RAII guard that records the
        // matching End on EVERY exit path, including future-drop/cancel
        // (Ctrl-C). Without it, a cancel between Start and the End call
        // would leak the in-flight count and skip the burned-CPU
        // accounting, false-tripping the breaker after enough cancels.
        let mut breaker_guard = if let Some(b) = &breaker {
            b.record(SpawnEvent::Start { proc_count: 1 });
            Some(BreakerSpanGuard::new(b.clone(), pgid))
        } else {
            None
        };
        let wall = Duration::from_millis(config.wall_timeout_ms);
        // Both entry points share one drain, so what a command printed is
        // kept on a stop either way. The streaming entry point passes a
        // counter for the live line count; the plain one passes none. The
        // wall-timeout wraps the drain; on timeout tokio cancels it → child
        // drops (kill_on_drop) → _tree_guard killpg, and the bytes already
        // read stay in collected for the caller.
        let collected = CapturedOutput::default();
        let outcome = timeout(
            wall,
            capture(
                child,
                lines.as_deref(),
                &collected,
                DRAIN_GRACE,
                receiver.as_deref(),
            ),
        )
        .await;
        // Measure the tree's CPU while _tree_guard still holds the group
        // alive. Drop order is reverse declaration: breaker_guard drops
        // BEFORE _tree_guard, so on the cancel path the tree is still
        // live when the guard measures. On a wall-timeout the spinning
        // descendants are still visible to ps; on a clean exit they were
        // already reaped and this reads ~0.
        let cpu_secs = breaker.as_ref().map(|_| tree_cpu_secs(pgid)).unwrap_or(0);
        let mut result = match outcome {
            Ok(Ok(output)) => Ok(ExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code(),
            }),
            Ok(Err(e)) => Err(SandboxError::Io(format!("sandbox-exec drain: {e}"))),
            Err(_elapsed) => Err(collected.timeout_error(timeout::message(config.wall_timeout_ms))),
        };
        if let Ok(exec) = &mut result {
            // The narrowed root is the one the profile allows; the original
            // workspace is refused in a worktree session, so naming it would
            // send the reader to a second denial.
            annotate_fence_refusal(exec, &self.effective_workspace(), &self.tmpdir);
        }
        // Feed the measured outcome to the guard; its Drop records End on
        // this path AND on the cancel path (where it measures CPU itself).
        // A user cancel is not a budget exceed (only Timeout /
        // ResourceLimitExceeded are) — it does not bump the consecutive
        // counter, though its burned CPU still lands in the aggregate.
        if let Some(g) = breaker_guard.as_mut() {
            let exceeded_budget = matches!(
                &result,
                Err(SandboxError::Timeout { .. } | SandboxError::ResourceLimitExceeded(_))
            );
            g.finish(cpu_secs, exceeded_budget);
        }
        result
        // Locals drop in reverse order: breaker_guard (records End) then
        // _tree_guard (killpg). On cancel, breaker_guard measures CPU
        // while the tree is still live, then _tree_guard reaps it.
    }
}
