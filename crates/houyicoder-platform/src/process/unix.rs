//! The process table probe on Unix.

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;

/// True when a process with this identifier exists.
///
/// Signal zero probes existence without delivering anything: success and a
/// permission refusal both mean the process exists, because the refusal is
/// about the signal right rather than the process. An unknown identifier means
/// it does not.
pub fn alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}
