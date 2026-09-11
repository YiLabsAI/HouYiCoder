//! Linux clipboard probe and cache.

#[cfg(target_os = "linux")]
use std::sync::Mutex;

#[cfg(target_os = "linux")]
use super::run_native;

/// Cached Linux clipboard tool. The probe runs on the first copy with the
/// actual text (xclip and xsel would hang on an empty probe stdin), then the
/// winner is cached so later copies skip the chain. wl-copy only when
/// WAYLAND_DISPLAY is set; xclip then xsel cover X11.
#[cfg(target_os = "linux")]
static LINUX_TOOL: Mutex<Option<Option<&'static str>>> = Mutex::new(None);

#[cfg(target_os = "linux")]
pub(super) fn linux_clipboard_tool(text: &str) -> Option<&'static str> {
    {
        let cache = LINUX_TOOL.lock().expect("linux tool cache");
        match *cache {
            Some(Some(tool)) => return Some(tool),
            Some(None) => return None,
            None => {}
        }
    }
    let winner = if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && run_native("wl-copy", &[], text).is_ok()
    {
        Some("wl-copy")
    } else if run_native("xclip", &["-selection", "clipboard"], text).is_ok() {
        Some("xclip")
    } else if run_native("xsel", &["--clipboard", "--input"], text).is_ok() {
        Some("xsel")
    } else {
        None
    };
    *LINUX_TOOL.lock().expect("linux tool cache") = Some(winner);
    winner
}
