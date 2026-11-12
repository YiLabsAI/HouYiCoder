//! The host domain: platform-agnostic value types plus a per-target reader
//! that fills them from the live desktop configuration.

pub mod net;

use net::ProxyResolution;

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
