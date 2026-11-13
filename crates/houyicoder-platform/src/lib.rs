//! Cross-platform abstraction layer (L0): the lowest workspace crate.
//!
//! Three capability domains live here so the rest of the workspace never holds
//! target-specific code: host runs on a headless container, desktop owns the
//! terminal and clipboard, and ipc owns local byte streams. Platform-specific
//! code is mounted per target so compiling one target never compiles another's
//! foreign function interface.

pub mod desktop;
pub mod host;
pub mod ipc;
pub mod process;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(target_os = "windows")]
mod windows;
