//! The signals that end a session without unwinding.
//!
//! A termination signal stops the process at the kernel boundary: no Rust
//! frame unwinds, so the terminal guard's Drop never runs and the emulator is
//! left reporting mouse motion into a shell that no longer reads it. The
//! handler here only raises a flag, and the run loop notices it on its next
//! turn and leaves through the normal teardown, so the guard restores every
//! mode it took. Nothing runs in the handler beyond an atomic store, which is
//! what keeps the signal context free of the allocation and locking a restore
//! would need.
//!
//! SIGKILL cannot be caught; a session stopped that way still leaks its modes,
//! and the terminal's own reset is the only recovery.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use signal_hook::consts::{SIGHUP, SIGQUIT, SIGTERM};
#[cfg(unix)]
use signal_hook::flag;

/// The one flag every watch reads. Signal registration is process-wide and
/// cannot be undone cheaply, so it happens once: a second registration would
/// leave a handler that nobody reads and that keeps its flag alive for the
/// life of the process. A session switch runs the loop again, and it must
/// share the flag the first registration set.
static REQUESTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// A flag raised when a termination signal arrives. The loop reads it once
/// per turn; the restore runs on the loop's normal exit path.
pub(crate) struct TerminationWatch {
    requested: Arc<AtomicBool>,
}

impl TerminationWatch {
    /// Whether a termination signal has arrived. Read once per loop turn so
    /// the teardown happens within one poll interval of the signal.
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }

    /// A watch in a stated state, for a caller testing one path without
    /// raising a real signal.
    #[cfg(test)]
    pub(crate) fn for_test(seen_signal: bool) -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(seen_signal)),
        }
    }

    /// Whether the run loop should leave. Both ends are the same decision:
    /// the user asked to quit, or a signal asked the process to stop, and
    /// either way the loop leaves through the teardown that restores the
    /// terminal.
    pub(crate) fn should_exit(&self, quit: bool) -> bool {
        quit || self.requested()
    }
}

/// Watch the signals a terminal session receives when its parent goes away or
/// an operator stops it. The watch lives for the whole run; the registration
/// is process-wide and stays installed, which is correct for a process that
/// serves exactly one session.
#[cfg(unix)]
pub(crate) fn watch_termination() -> std::io::Result<TerminationWatch> {
    if let Some(requested) = REQUESTED.get() {
        return Ok(TerminationWatch {
            requested: Arc::clone(requested),
        });
    }
    let requested = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGHUP, SIGQUIT] {
        flag::register(signal, Arc::clone(&requested))?;
    }
    let shared = Arc::clone(REQUESTED.get_or_init(|| requested));
    Ok(TerminationWatch { requested: shared })
}

/// A platform with no signal to watch keeps the same shape, so the run loop
/// needs no branch. No handler is installed, so the flag is never raised by
/// the platform; every watch still reads the one shared flag rather than a
/// fresh one, because a session switch runs the loop again and the watches
/// must not disagree. A Windows console close event is delivered through the
/// console API, which this layer does not own.
#[cfg(not(unix))]
pub(crate) fn watch_termination() -> std::io::Result<TerminationWatch> {
    let shared = Arc::clone(REQUESTED.get_or_init(|| Arc::new(AtomicBool::new(false))));
    Ok(TerminationWatch { requested: shared })
}

#[cfg(test)]
mod tests;
