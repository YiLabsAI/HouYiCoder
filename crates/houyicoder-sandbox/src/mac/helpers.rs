//! Free helper functions for the macOS Seatbelt session: temp-dir minting,
//! process-group kill, and tree-CPU measurement. Split from mac.rs so that
//! file stays under the file-size gate; the session struct, its RAII guards,
//! and the exec path remain in mac.rs.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_context::SandboxError;

/// Counter for unique temp-dir names within this process.
pub(super) static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mint a fresh temp directory under the system temp root with a unique
/// per-process, per-counter, per-nanos name. Used for both the session
/// workspace (a sandboxed cwd this session owns) and the per-session tmp dir
/// (allow-listed in the profile, exported as TMPDIR).
pub(super) fn mkdtemp(prefix: &str) -> Result<PathBuf, SandboxError> {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!("{prefix}-{}-{n}-{nanos}", std::process::id()));
    std::fs::create_dir(&path)?;
    Ok(path)
}

/// Kill the whole process group (SIGKILL). No-op on pgid <= 0. Best-effort.
/// This is the tree-kill that prevents orphan processes: grandchildren
/// survive a direct child.kill() but not a killpg.
pub(super) fn kill_process_group(pgid: i32) {
    if pgid > 0 {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

/// Best-effort total CPU seconds of a process group, via ps. Captures the
/// direct child plus every descendant in the pgid at the moment of the call,
/// so a runaway grandchild (the orphan-CPU class a direct-child rlimit would
/// miss) is counted while the tree is still alive. Used after a wall-timeout
/// while the tree-kill guard holds the group; on a clean exit the procs are
/// already reaped and this reads ~0. Any failure (ps missing, non-zero exit,
/// a bad row) returns 0 so a measurement fault never breaks exec — the
/// breaker's in-flight-proc + consecutive-fail trips are the reliable
/// backstops, independent of CPU measurement.
///
/// Known gap: a grandchild that double-forks and calls setsid escapes the
/// process group and is invisible here. The orphan-CPU threat (a tool
/// command that spins CPU in a descendant) does not daemonize, so this is
/// acceptable against that threat; a true daemon-escape needs cgroup
/// accounting (Linux) or a userspace watchdog polling the session, tracked
/// separately.
// Migration: route this spawn through ProcessLauncher. Tracked as a known
// direct spawn until the migration (a ps child to measure tree CPU).
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
pub(super) fn tree_cpu_secs(pgid: i32) -> u64 {
    if pgid <= 0 {
        return 0;
    }
    let out = match std::process::Command::new("ps")
        .arg("-A")
        .arg("-o")
        .arg("pgid=")
        .arg("-o")
        .arg("cputime=")
        .output()
    {
        Ok(o) => o,
        Err(_) => return 0,
    };
    if !out.status.success() {
        return 0;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut total: u64 = 0;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let group = match parts.next().and_then(|s| s.parse::<i32>().ok()) {
            Some(v) => v,
            None => continue,
        };
        if group != pgid {
            continue;
        }
        let Some(t) = parts.next() else { continue };
        total = total.saturating_add(parse_cputime_secs(t));
    }
    total
}

/// Parse a ps cputime field into seconds. Layouts vary by platform
/// (mac: M:SS.ss with unbounded minutes; linux: [[dd-]hh:]mm:ss). Splitting
/// on the colon and folding left-to-right with a x60 weight handles any
/// segment count; f64 absorbs the fractional seconds mac emits. A malformed
/// segment is skipped (the accumulator is unchanged) so one bad row cannot
/// corrupt the sum.
fn parse_cputime_secs(s: &str) -> u64 {
    let secs = s.split(':').fold(0.0_f64, |acc, part| {
        part.parse::<f64>().map_or(acc, |n| acc * 60.0 + n)
    });
    if secs > 0.0 { secs as u64 } else { 0 }
}

/// RAII guard that decrements the in-flight exec count on drop, so every
/// exit path (Ok, Err, wall-timeout, future-drop/cancel) keeps the count
/// balanced. A cancel between the fetch_add and this guard's drop is the one
/// leak window — acceptable for the best-effort worktree-enter gate.
pub(super) struct ExecCountGuard(pub(super) std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Drop for ExecCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
