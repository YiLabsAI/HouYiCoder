//! The signals that end a session without unwinding.
//!
//! A termination signal stops the process at the kernel boundary: no Rust
//! frame unwinds, so the terminal guard's Drop never runs. The handler only
//! writes a byte to a self-pipe; a thread reading it raises the flag and wakes
//! the run loop, which leaves through the normal teardown. SIGKILL cannot be
//! caught, so a session stopped that way still leaks its modes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

#[cfg(unix)]
use std::sync::Mutex;

#[cfg(unix)]
use signal_hook::consts::{SIGHUP, SIGQUIT, SIGTERM};
#[cfg(unix)]
use signal_hook::iterator::Signals;
#[cfg(unix)]
use std::thread::Builder;

#[cfg(unix)]
use crate::app::wake::ui_wake;

/// The one flag every watch reads. Signal registration is process-wide and
/// cannot be undone cheaply, so it happens once: a second registration would
/// leave a handler that nobody reads and that keeps its flag alive for the
/// life of the process. A session switch runs the loop again, and it must
/// share the flag the first registration set.
static REQUESTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Serializes the check-then-install of the signal registration. Two callers
/// racing into it would otherwise each register a handler, and one of the two
/// registrations would outlive the session that installed it.
#[cfg(unix)]
static REGISTRATION: Mutex<()> = Mutex::new(());

/// A flag raised when a termination signal arrives. The loop reads it once
/// per turn; the restore runs on the loop's normal exit path.
pub(crate) struct TerminationWatch {
    requested: Arc<AtomicBool>,
}

impl TerminationWatch {
    /// Whether a termination signal has arrived. Read once per loop turn so
    /// the teardown happens within one poll interval of the signal.
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
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
    // The guard is held across registration and thread start so a second
    // caller waits and then joins the flag the first one installed, instead
    // of registering a handler of its own.
    let _registration = REGISTRATION.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(requested) = REQUESTED.get() {
        return Ok(TerminationWatch {
            requested: Arc::clone(requested),
        });
    }
    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGQUIT])?;
    let requested = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&requested);
    Builder::new()
        .name("termination".to_string())
        .spawn(move || {
            // The store and the wake run here, on a normal thread, so the
            // signal context stays a single pipe write. The store is
            // sequentially consistent so a loop that reads the flag without
            // waking sees the signal that was already raised.
            for _ in signals.forever() {
                signal_flag.store(true, Ordering::SeqCst);
                ui_wake().wake();
            }
        })?;
    // Published only after the thread exists: a failed spawn must not leave a
    // flag behind that no handler will ever raise. The lock above guarantees
    // the slot is still empty.
    let _ = REQUESTED.get_or_init(|| Arc::clone(&requested));
    Ok(TerminationWatch { requested })
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
