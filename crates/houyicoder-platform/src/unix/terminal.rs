//! Unix terminal boundary: the signals that end a session and the descriptor
//! probe that sees a terminal go away. Migrated from the terminal layer
//! above so that layer holds no platform conditionals; the behavior is
//! unchanged, including the single shared registration and the wake that
//! follows the flag.

use std::fs::File;
use std::io::IsTerminal;
use std::io::stdin;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::Builder;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use signal_hook::consts::{SIGHUP, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;

/// Serializes the check-then-install of the signal registration. Two callers
/// racing into it would otherwise each register a handler, and one of the
/// registrations would outlive the session that installed it.
static REGISTRATION: Mutex<()> = Mutex::new(());

/// The wake the signal thread raises, handed in at registration. Held as a
/// boxed callback so this crate stays free of a dependency on the layer
/// above.
static WAKE: Mutex<Option<Box<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Probe the process's terminal for a hangup.
///
/// A closed terminal leaves its descriptor readable with nothing to read,
/// which the event reader answers by reading again. Asking the descriptor
/// directly is the only check that does not depend on a signal arriving.
pub(crate) fn probe_hangup() -> bool {
    probe(watched_terminal())
}

/// The terminal the event reader reads, or None when the process has none.
///
/// The reader takes standard input when that is a terminal and the
/// controlling terminal otherwise, so a process fed by a pipe still reads a
/// terminal. A pipe whose writer exited is end-of-file, not a dead terminal,
/// which is why standard input is only watched when it is the descriptor in
/// use.
fn watched_terminal() -> Option<BorrowedFd<'static>> {
    static WATCHED: OnceLock<Option<OwnedFd>> = OnceLock::new();
    if stdin().is_terminal() {
        return None;
    }
    WATCHED
        .get_or_init(|| {
            File::options()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .ok()
                .map(OwnedFd::from)
        })
        .as_ref()
        .map(|tty| tty.as_fd())
}

/// Whether the watched descriptor reported a hangup.
fn probe(tty: Option<BorrowedFd<'_>>) -> bool {
    let input = stdin();
    let fd = match &tty {
        Some(tty) => tty.as_fd(),
        None if input.is_terminal() => input.as_fd(),
        None => return false,
    };
    let mut fds = [PollFd::new(
        fd,
        PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
    )];
    poll(&mut fds, PollTimeout::ZERO).is_ok()
        && fds[0]
            .revents()
            .is_some_and(|flags| flags.intersects(PollFlags::POLLHUP | PollFlags::POLLERR))
}

/// Register the signals a terminal session receives when its parent goes
/// away or an operator stops it, and the wake to raise when one arrives.
///
/// The registration is process-wide and stays installed: a second caller
/// waits for the lock and then joins the flag the first one installed
/// rather than registering a handler of its own. The store and the wake run
/// on a normal thread, so the signal context stays a single pipe write.
pub(crate) fn watch_signals(
    requested: Arc<AtomicBool>,
    wake: impl Fn() + Send + Sync + 'static,
) -> std::io::Result<()> {
    // A session switch runs the loop again and must read the same flag the
    // first registration raised, so the registration happens once. The wake
    // is refreshed each call so a new session's loop wakes on the signal.
    static REGISTERED: OnceLock<()> = OnceLock::new();
    *WAKE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(wake));
    if REGISTERED.get().is_some() {
        return Ok(());
    }
    let _registration = REGISTRATION.lock().unwrap_or_else(|e| e.into_inner());
    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGQUIT])?;
    let signal_flag = Arc::clone(&requested);
    // The thread is spawned before the caller returns, so a failed spawn
    // surfaces as an error and no flag is left behind unread.
    Builder::new()
        .name("termination".to_string())
        .spawn(move || {
            for _ in signals.forever() {
                signal_flag.store(true, Ordering::SeqCst);
                raise_wake();
            }
        })?;
    let _ = REGISTERED.set(());
    Ok(())
}

/// Raise the wake the registration was handed, if it still holds one.
fn raise_wake() {
    let wake = WAKE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(wake) = wake.as_ref() {
        wake();
    }
}

#[cfg(test)]
mod tests;
