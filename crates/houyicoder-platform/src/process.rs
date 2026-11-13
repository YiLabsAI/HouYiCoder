//! Process liveness: whether an identifier still names a running process.
//!
//! Callers hold a recorded identifier, not a handle they opened, so the probe
//! answers from the process table and asks nothing of the process itself. The
//! answer is that the identifier is in use, not that the process which
//! recorded it is still the holder: a reused identifier reads alive.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix::alive as platform_alive;
#[cfg(windows)]
use windows::alive as platform_alive;

/// True when a process with this identifier exists.
///
/// A non-positive identifier names no individual process, so it is never
/// alive. The answer comes before the platform probe, where zero and negative
/// values address a process group rather than one process.
pub fn alive(pid: i32) -> bool {
    pid > 0 && platform_alive(pid)
}

#[cfg(test)]
mod tests;
