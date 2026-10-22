//! Process-group teardown and accounting for the macOS Seatbelt session: the
//! tree kill, the CPU total a timeout reports, and the in-flight exec counter.

use nix::sys::signal::Signal;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Kill the whole process group. A direct child kill leaves grandchildren
/// running, which is how a timed-out command keeps burning CPU.
pub(super) fn kill_process_group(pgid: i32) {
    if pgid > 0 {
        let _ = kill(Pid::from_raw(-pgid), Signal::SIGKILL);
    }
}

/// Total CPU seconds of a process group, summed over the processes alive at the
/// call. Best effort: a missing or failing ps reports zero rather than breaking
/// exec, since the in-flight and consecutive-failure limits still bound a
/// runaway command.
///
/// A grandchild that double-forks and calls setsid leaves the group and is not
/// counted here.
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

/// Parse a ps cputime field into seconds. macOS prints M:SS.ss with unbounded
/// minutes and Linux prints [[dd-]hh:]mm:ss, so folding the colon-separated
/// segments at 60 handles either layout.
pub(super) fn parse_cputime_secs(s: &str) -> u64 {
    let secs = s.split(':').fold(0.0_f64, |acc, part| {
        part.parse::<f64>().map_or(acc, |n| acc * 60.0 + n)
    });
    if secs > 0.0 { secs as u64 } else { 0 }
}

/// Decrements the in-flight exec count on drop, so every exit path keeps the
/// count balanced.
pub(super) struct ExecCountGuard(pub(super) Arc<AtomicU64>);

impl Drop for ExecCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
