//! How a sandboxed command runs: the shell it is handed to, and the note its
//! result carries when the fence refused a path.

use houyicoder_context::ExecResult;
use std::env::var;
use std::path::Path;
use std::path::PathBuf;

/// Shells a sandboxed command runs under, best first. The model writes bash,
/// so running the user's login shell applies zsh parsing rules to bash text and
/// aborts on constructs like a leading === or a read-only status. A newer bash
/// from a package manager keeps associative arrays and mapfile; the system bash
/// is the floor. The login shell is the last resort, for a host with no bash.
pub(super) const PREFERRED_SHELLS: &[&str] =
    &["/opt/homebrew/bin/bash", "/usr/local/bin/bash", "/bin/bash"];

/// The first candidate that exists, or the caller's fallback. Split from the
/// environment read so the fallback path is reachable in a test.
pub(super) fn first_present(
    candidates: &[&str],
    present: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|candidate| present(Path::new(candidate)))
        .map(PathBuf::from)
}

/// The login shell the host advertises, when it advertises one.
pub(super) fn login_shell() -> Option<PathBuf> {
    var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

pub(super) fn command_shell() -> PathBuf {
    first_present(PREFERRED_SHELLS, |path| path.is_file())
        .or_else(login_shell)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// The wall-clock budget as the model reads it: seconds, never the raw
/// millisecond count.
pub(super) fn timeout_message(millis: u64) -> String {
    format!("wall-clock {}s exceeded", millis / 1000)
}

/// Append an actionable note when the kernel refused the command. The raw
/// message names a path and nothing else, which reads as a broken tool rather
/// than a boundary, so the note states where the session may write.
pub(super) fn annotate_fence_refusal(exec: &mut ExecResult, workspace: &Path, tmpdir: &Path) {
    if exec.is_success() || !exec.stderr.contains("Operation not permitted") {
        return;
    }
    exec.stderr.push_str(&format!(
        "\n[houyicoder] the sandbox refused this path. Writable roots: {} and TMPDIR={}.\n",
        workspace.display(),
        tmpdir.display()
    ));
}
