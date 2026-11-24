//! Cross-platform abstraction layer (L0): the lowest workspace crate.
//!
//! Four capability domains live here so the rest of the workspace never holds
//! target-specific code: host runs on a headless container, desktop owns the
//! terminal and clipboard, ipc owns local byte streams, and tls installs the
//! process-wide crypto backend. Platform-specific code is mounted per target
//! so compiling one target never compiles another's foreign function
//! interface.

pub mod desktop;
pub mod host;
pub mod ipc;
pub mod process;
pub mod tls;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(target_os = "windows")]
mod windows;
