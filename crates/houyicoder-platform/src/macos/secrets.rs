//! Reads a stored secret from the login keychain through the system tool.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::host::secrets::{SecretEncoding, SecretRead, decode_secret};
use crate::macos::wait_bounded;

/// Absolute path, never a bare program name: resolving this through the path
/// would let anything earlier on it answer for the keychain.
const SECURITY_TOOL: &str = "/usr/bin/security";

/// How long a lookup may run. The keychain can put an authorization dialog in
/// front of a human, so the bound is wide; it is finite because startup waits
/// on the answer.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);

/// What one run of the tool produced.
enum Lookup {
    /// The tool answered with the item's password on standard output.
    Output(Vec<u8>),
    /// The tool ran and reported no item: a missing entry and a denied
    /// authorization both leave it with a non-zero exit.
    Missing,
    /// The tool could not be run, or did not finish in time.
    Failed,
}

pub(crate) fn read_secret(service: &str, account: &str) -> SecretRead {
    // A keychain read is the event an operator should be able to see: it
    // names the item asked for, never the value it holds.
    tracing::info!(service, account, "reading keychain item");
    match lookup(SECURITY_TOOL, &query_args(service, account)) {
        Lookup::Output(bytes) => parse_security_output(&bytes),
        Lookup::Missing => {
            tracing::warn!(service, account, "keychain item missing or access denied");
            SecretRead::NotFound
        }
        Lookup::Failed => {
            tracing::warn!(service, account, "keychain read failed or timed out");
            SecretRead::Unavailable
        }
    }
}

/// The arguments that name the item and ask for the password alone. Dropping
/// the account would return whichever entry the keychain answers with;
/// dropping the password flag would return a description instead.
fn query_args<'a>(service: &'a str, account: &'a str) -> [&'a str; 6] {
    ["find-generic-password", "-s", service, "-a", account, "-w"]
}

/// Turn the tool's standard output into a read result.
///
/// The tool terminates the password with one newline, and that terminator is
/// a property of its output rather than of the key. Nothing else is trimmed:
/// a value with other whitespace in it is rejected, so a key that reaches the
/// provider is the key the keychain holds.
fn parse_security_output(bytes: &[u8]) -> SecretRead {
    let value = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let value = value.strip_suffix(b"\r").unwrap_or(value);
    match decode_secret(value, SecretEncoding::Utf8) {
        Some(key) => SecretRead::Value(key),
        None => SecretRead::InvalidValue,
    }
}

/// Run the tool with the given arguments and read its answer within the
/// bounded window. The program is a parameter so the process handling can be
/// driven by a trivial child; only the real lookup needs the keychain.
#[expect(
    clippy::disallowed_methods,
    reason = "L0 platform probe below the ProcessLauncher port; no runtime dependency"
)]
fn lookup(program: &str, args: &[&str]) -> Lookup {
    let mut child = match Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Lookup::Failed,
    };
    // A child that fills the pipe buffer would block before it exits, so the
    // output is drained on its own thread while the deadline runs here.
    let drain = child.stdout.take().map(|mut reader| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            drop(reader.read_to_end(&mut buf));
            buf
        })
    });
    let status = wait_bounded(&mut child, LOOKUP_TIMEOUT);
    drop(child.kill());
    drop(child.wait());
    let bytes = drain
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    match status {
        Some(status) if status.success() => Lookup::Output(bytes),
        Some(_) => Lookup::Missing,
        None => Lookup::Failed,
    }
}

#[cfg(test)]
mod tests;
