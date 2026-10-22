//! Free helper functions for the macOS Seatbelt session: temp-dir minting,
//! process-group kill, and tree-CPU measurement. Split from mac.rs so that
//! file stays under the file-size gate; the session struct, its RAII guards,
//! and the exec path remain in mac.rs.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_context::SandboxError;
use nix::sys::signal::Signal;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::env::temp_dir;
use std::fs::create_dir;
use std::io::ErrorKind;
use std::process::Command;
use std::process::id;
use std::sync::Arc;

/// Counter for unique temp-dir names within this process.
pub(super) static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Longest temp-directory path this module will hand out, in bytes. Build
/// tools put their Unix-domain socket under TMPDIR, and those paths are capped
/// by SUN_LEN (104 bytes on macOS). A session temp path that eats most of that
/// budget fails every build in the session with an error naming the socket
/// rather than the temp dir, so the minted path leaves room for a socket name.
///
/// Asserted in the tests rather than enforced at mint time: a host whose temp
/// root is already long cannot be fixed by refusing to start a session.
#[cfg(test)]
pub(super) const MAX_TEMP_PATH_BYTES: usize = 80;

/// Mint a fresh temp directory under the system temp root. Used for both the
/// session workspace (a sandboxed cwd this session owns) and the per-session
/// tmp dir (allow-listed in the profile, exported as TMPDIR).
///
/// The leaf is deliberately short: pid in hex plus a per-process counter. That
/// is unique within a boot, and create_dir is the collision check, so a name
/// left behind by a crashed process with the same pid advances the counter
/// instead of failing the session.
pub(super) fn mkdtemp(prefix: &str) -> Result<PathBuf, SandboxError> {
    let base = temp_dir();
    loop {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = base.clone();
        path.push(format!("{prefix}-{}-{n:x}", id()));
        match create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

/// Kill the whole process group (SIGKILL). No-op on pgid <= 0. Best-effort.
/// This is the tree-kill that prevents orphan processes: grandchildren
/// survive a direct child.kill() but not a killpg.
pub(super) fn kill_process_group(pgid: i32) {
    if pgid > 0 {
        let _ = kill(Pid::from_raw(-pgid), Signal::SIGKILL);
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
    let out = match Command::new("ps")
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
pub(super) struct ExecCountGuard(pub(super) Arc<AtomicU64>);

impl Drop for ExecCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
