//! Capability gate + native OS sandbox.
//!
//! Per-platform backends behind one SandboxSession trait:
//! - macOS Seatbelt via sandbox-exec: a real kernel-level fence (deny
//!   default, allow workspace + system binaries, deny egress by default).
//! - Linux Landlock: a per-spawn path fence applied by a sibling binary the
//!   session spawns for every command; hosts where the fence binary or the
//!   kernel fence is unavailable degrade to an audited resolver-only
//!   boundary. The binary must be installed beside the daemon;
//!   HOUYICODER_SANDBOX_HELPER overrides discovery, and a missing one
//!   reports a failed fence.
//! - Windows Job Object: fences the started tree's lifetime (kill on close).
//!   It applies no CPU, memory, or process-count limit yet, and carries no
//!   path primitive, so its coverage stays Unfenced and the resolver remains
//!   the boundary.
//!
//! Path canonicalization goes through dunce so the Windows UNC prefix std
//! canonicalize yields does not break downstream string ops; on unix it
//! matches std behavior. The nix dependency (process-group signal) is
//! target-gated to unix so the crate compiles on Windows too.

#[cfg(target_os = "macos")]
mod profile;

#[cfg(target_os = "macos")]
pub use profile::{
    ProfileSpec, allow_home_gitconfig, allow_set, allow_system_etc, deny_default, deny_read_always,
    filesystem_rules, render, render_profile,
};

#[cfg(target_os = "macos")]
mod shell_snapshot;
#[cfg(target_os = "macos")]
pub use shell_snapshot::ShellSnapshot;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::MacSeatbeltSession;
#[cfg(target_os = "macos")]
pub use mac::command_shell;

pub mod deny_log;

mod shell_selection;
pub use shell_selection::{ShellPreference, ShellProgram, ShellSelection, select_shell};

mod output_capture;
mod runtime_dirs;
mod timeout;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::LinuxLandlockSession;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsJobSession;

use std::env::var;
use std::sync::atomic::{AtomicBool, Ordering};

static UNFENCED_OVERRIDE: AtomicBool = AtomicBool::new(false);

/// Explicitly disable the operating system kernel sandbox fence for the session.
pub fn set_unfenced(unfenced: bool) {
    UNFENCED_OVERRIDE.store(unfenced, Ordering::SeqCst);
}

/// Whether the sandbox operates without the operating system kernel fence
/// (via the --no-sandbox CLI flag or the HOUYICODER_SANDBOX_NO_ENFORCE environment variable).
pub fn is_unfenced() -> bool {
    UNFENCED_OVERRIDE.load(Ordering::Relaxed)
        || var("HOUYICODER_SANDBOX_NO_ENFORCE").is_ok_and(|v| v == "1")
}

/// The native sandbox session for the host platform. Each backend implements
/// SandboxSession; callers get the right kernel-level fence (or the audited
/// no-op v1 on platforms whose real backend is still pending) without a cfg
/// maze of their own.
#[cfg(target_os = "macos")]
pub type PlatformSession = MacSeatbeltSession;
#[cfg(target_os = "linux")]
pub type PlatformSession = LinuxLandlockSession;
#[cfg(target_os = "windows")]
pub type PlatformSession = WindowsJobSession;
