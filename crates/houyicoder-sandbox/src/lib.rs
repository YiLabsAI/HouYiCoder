//! Capability gate + native OS sandbox.
//!
//! Per-platform backends behind one SandboxSession trait:
//! - macOS Seatbelt via sandbox-exec: a real kernel-level fence (deny
//!   default, allow workspace + system binaries, deny egress by default).
//! - Linux Landlock: v1 is an audited no-op. A real Landlock fence is a
//!   tracked follow-up; the landlock crate API is large enough to warrant a
//!   dedicated design pass before adoption, so v1 does not pull it in.
//! - Windows Job Object: v1 is an audited no-op. A real Job Object fence is
//!   a tracked follow-up.
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

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::LinuxLandlockSession;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsJobSession;

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
