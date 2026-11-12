//! The events that end a session without unwinding.
//!
//! The registration and the flags live in the platform layer, which owns the
//! per-target boundary; this module is the re-export the run loop reads. A
//! termination event ends the process without unwinding, so the terminal
//! guard's Drop never runs; the platform raises the flag and the wake, and
//! the loop leaves through its normal teardown instead.

pub(crate) use houyicoder_platform::desktop::terminal::TerminationWatch;
pub(crate) use houyicoder_platform::desktop::terminal::watch_termination;

#[cfg(test)]
mod tests;
