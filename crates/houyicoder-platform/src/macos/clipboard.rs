//! The macOS clipboard utility: the absolute path to the copy tool and the
//! call that feeds it.

use std::io::Result;

use crate::desktop::clipboard::run_utility;

/// The absolute path to the macOS clipboard utility, never a bare name: a bare
/// name is resolved through the search path, and a directory earlier on it
/// could answer with a program the copy never meant to run.
pub(crate) const CLIP_TOOL: &str = "/usr/bin/pbcopy";

/// Copy through the native utility. The caller uses the result to decide
/// whether to also emit the terminal escape: a clean run is the whole copy,
/// and a failure is the reason to fall back.
pub(crate) fn copy(text: &str) -> Result<()> {
    run_utility(CLIP_TOOL, &[], text)
}
