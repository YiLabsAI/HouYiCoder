//! macOS system readers: the desktop proxy settings and the login keychain.

mod net;
mod secrets;

use std::process::{Child, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) use net::read_system_proxy;
pub(crate) use secrets::read_secret;

#[cfg(test)]
mod tests;

/// Poll a child until it exits or the deadline passes. A poll loop rather
/// than a blocking wait keeps the deadline authoritative: the system tools
/// this bounds can wait on a human or on a service that never answers.
pub(crate) fn wait_bounded(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(1));
    }
}
