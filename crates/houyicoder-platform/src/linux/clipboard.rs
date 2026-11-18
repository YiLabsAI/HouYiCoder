//! The Linux clipboard utilities: the probe that picks one, cached after the
//! first copy.

use std::env::var_os;
use std::io::{Error, Result};
use std::sync::Mutex;

use crate::unix::clipboard::run_utility_unix;

/// The utility that answered last, cached so a copy costs no probe.
static ANSWERING_TOOL: Mutex<Option<Option<&'static str>>> = Mutex::new(None);

/// Copy through the utility that answers, if any. The caller uses the result
/// to decide whether to also emit the terminal escape.
pub(crate) fn copy(text: &str) -> Result<()> {
    if let Some((tool, args)) = answering_tool(text) {
        run_utility_unix(tool, args, text)
    } else {
        Err(Error::other("no clipboard utility answered"))
    }
}

/// The utility that answers, cached after the first copy. The probe runs
/// with the actual text because an empty stdin leaves some of these waiting
/// for input that never comes.
fn answering_tool(text: &str) -> Option<(&'static str, &'static [&'static str])> {
    let cached = ANSWERING_TOOL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(winner) = *cached {
        return winner.map(|tool| (tool, arguments(tool)));
    }
    drop(cached);
    let wayland = var_os("WAYLAND_DISPLAY").is_some();
    let winner = if wayland && run_utility_unix("wl-copy", &[], text).is_ok() {
        Some("wl-copy")
    } else if run_utility_unix("xclip", &["-selection", "clipboard"], text).is_ok() {
        Some("xclip")
    } else if run_utility_unix("xsel", &["--clipboard", "--input"], text).is_ok() {
        Some("xsel")
    } else {
        None
    };
    *ANSWERING_TOOL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(winner);
    winner.map(|tool| (tool, arguments(tool)))
}

/// The arguments each utility takes to reach the clipboard. A tool with no
/// entry reads the clipboard from its plain invocation.
fn arguments(tool: &str) -> &'static [&'static str] {
    match tool {
        "xclip" => &["-selection", "clipboard"],
        "xsel" => &["--clipboard", "--input"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests;
