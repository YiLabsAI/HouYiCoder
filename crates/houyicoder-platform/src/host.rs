//! The host domain: platform-agnostic value types plus a per-target reader
//! that fills them from the live desktop configuration.

pub mod fs;
pub mod net;
pub mod secrets;
pub mod shell;

use std::env::{current_dir, temp_dir};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use net::ProxyResolution;
use shell::{HelperShell, ShellPreference, ShellProgram, ShellSelection};

#[cfg(target_os = "linux")]
use crate::linux::read_system_proxy as platform_proxy;
#[cfg(target_os = "macos")]
use crate::macos::read_system_proxy as platform_proxy;
#[cfg(target_os = "windows")]
use crate::windows::read_system_proxy as platform_proxy;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn platform_proxy() -> ProxyResolution {
    ProxyResolution::direct()
}

/// The proxy configuration the process should use right now: an explicit
/// environment configuration wins, then the desktop system settings.
pub(crate) fn system_proxy() -> ProxyResolution {
    net::env_proxy().unwrap_or_else(platform_proxy)
}

#[cfg(unix)]
use crate::unix::shell::discover as platform_discover;
#[cfg(target_os = "windows")]
use crate::windows::shell::discover as platform_discover;

/// The command interpreter path. Windows resolves it from the system directory;
/// other targets answer a name that is never used there, because no command
/// interpreter runs on them.
#[cfg(target_os = "windows")]
pub(crate) use crate::windows::shell::command_interpreter;
#[cfg(not(target_os = "windows"))]
pub(crate) fn command_interpreter() -> PathBuf {
    PathBuf::from("cmd")
}

/// The shell a host will run commands with, from the settings and the host's
/// own search. The untrusted directories name where a checkout, a working
/// directory, or a temp root could supply a program.
pub fn discover_shell(
    preference: ShellPreference,
    configured: Option<&Path>,
    untrusted: &[PathBuf],
) -> ShellSelection {
    platform_discover(preference, configured, untrusted)
}

/// The default shell choice, computed once and shared by every caller so the
/// search runs a single time in a process. A settings value that names a shell
/// is not the default and is resolved through discover_shell instead.
pub fn default_shell() -> ShellSelection {
    static DEFAULT: OnceLock<ShellSelection> = OnceLock::new();
    DEFAULT
        .get_or_init(|| {
            let untrusted = shell::untrusted_roots(&[], current_dir().ok().as_deref(), &temp_dir());
            discover_shell(ShellPreference::default(), None, &untrusted)
        })
        .clone()
}

/// Resolve a helper shell to the program that runs it. The sh variant is the
/// POSIX shell on every host; the Windows-only variants resolve on Windows and
/// answer None elsewhere, so a settings value a host cannot honor is reported
/// rather than silently replaced.
pub fn helper_program(shell: HelperShell) -> Option<ShellProgram> {
    #[cfg(target_os = "windows")]
    {
        return crate::windows::shell::helper_program(shell);
    }
    #[cfg(not(target_os = "windows"))]
    {
        match shell {
            HelperShell::Sh => Some(ShellProgram::Posix),
            _ => None,
        }
    }
}
