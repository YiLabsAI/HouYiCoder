//! Reads the Windows system proxy through the WinHttp helper, which reports
//! the live configuration flags. A disabled automatic script leaves no stale
//! address read as enabled, unlike a raw registry read that keeps the string.

use std::ffi::c_void;
use std::mem::zeroed;
use std::slice::from_raw_parts;

use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
use windows::Win32::Networking::WinHttp::{
    WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WinHttpGetIEProxyConfigForCurrentUser,
};

use crate::host::net::{ProxyConfig, ProxyResolution, expand_bypass_rules, parse_proxy_endpoints};

/// The proxy configuration the desktop reports right now. The helper fills
/// only the fields its toggles enable, so a closed automatic script does not
/// leave a stale address behind.
#[expect(
    unsafe_code,
    reason = "WinHttp returns pointers the caller must read and free"
)]
pub(crate) fn read_system_proxy() -> ProxyResolution {
    // The struct is a flag plus three pointers; zeroing it gives null pointers
    // the helper overwrites on success.
    let mut config: WINHTTP_CURRENT_USER_IE_PROXY_CONFIG = unsafe { zeroed() };
    if unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut config) }.is_err() {
        return ProxyResolution::direct();
    }
    let proxy = unsafe { take_wide_string(config.lpszProxy.0 as *const u16) };
    let bypass = unsafe { take_wide_string(config.lpszProxyBypass.0 as *const u16) };
    let auto_config_url = unsafe { take_wide_string(config.lpszAutoConfigUrl.0 as *const u16) };
    unsafe { free_string(config.lpszProxy.0) };
    unsafe { free_string(config.lpszProxyBypass.0) };
    unsafe { free_string(config.lpszAutoConfigUrl.0) };
    build_resolution(proxy, bypass, auto_config_url)
}

#[expect(
    unsafe_code,
    reason = "reads a null-terminated wide string the helper allocated"
)]
unsafe fn take_wide_string(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let chars = unsafe { from_raw_parts(ptr, len) };
    Some(String::from_utf16_lossy(chars))
}

#[expect(unsafe_code, reason = "frees a string the helper allocated")]
unsafe fn free_string(ptr: *mut u16) {
    if ptr.is_null() {
        return;
    }
    drop(unsafe { GlobalFree(Some(HGLOBAL(ptr as *mut c_void))) });
}

fn build_resolution(
    proxy: Option<String>,
    bypass: Option<String>,
    auto_config_url: Option<String>,
) -> ProxyResolution {
    let static_proxy = proxy
        .and_then(|raw| parse_proxy_endpoints(&raw))
        .map(|(http, https)| {
            let exceptions = expand_bypass_rules(bypass.as_deref().unwrap_or(""));
            ProxyConfig::new(http, https, exceptions)
        });
    if let Some(config) = static_proxy {
        return ProxyResolution::system(config);
    }
    // A static proxy wins. An explicit automatic-configuration script the
    // client cannot evaluate is a diagnostic. Mere auto-detection without a
    // script URL is direct: browsers do the same on a network with no WPAD
    // server, and treating it as an error would block every default install.
    if let Some(url) = auto_config_url
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
    {
        return ProxyResolution::pac(url);
    }
    ProxyResolution::direct()
}

#[cfg(test)]
mod tests;
