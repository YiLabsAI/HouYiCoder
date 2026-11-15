//! The Windows clipboard utility: the absolute path resolved from the system
//! root, so the working directory never answers the name.

use std::env::var_os;
use std::ffi::OsStr;
use std::path::PathBuf;

use crate::desktop::clipboard::run_utility;

/// Copy through the native utility when the system names its root. The
/// caller also emits the terminal escape, so an unset root costs nothing.
pub(crate) fn copy(text: &str) {
    if let Some(program) = clip_program(var_os("SystemRoot").as_deref()) {
        drop(run_utility(&program, &[], text));
    }
}

/// The absolute path of the Windows clipboard utility under the given system
/// root. A bare name would be resolved through the search path, and a
/// directory earlier on it could answer with a program the copy never meant
/// to run.
fn clip_program(root: Option<&OsStr>) -> Option<String> {
    let root = root?;
    let mut path = PathBuf::from(root);
    path.push("System32");
    path.push("clip.exe");
    Some(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests;
