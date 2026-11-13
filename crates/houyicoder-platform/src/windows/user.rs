//! The current account's security identifier as a string, and a draw from the
//! system random source. Both feed the naming and the security descriptor a
//! session pipe is created with, so they stay next to each other rather than
//! in the pipe module, which is portable.

use std::io::{Error, Result};
use std::mem::size_of;
use std::slice::from_raw_parts;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PWSTR;

use crate::ipc::NONCE_LEN;

/// The security identifier of the account this process runs as, spelled as
/// the string form a security descriptor accepts. It names an account
/// without depending on the display name, which varies by locale.
#[expect(unsafe_code, reason = "reads the process token's user identifier")]
pub(crate) fn current_sid_string() -> Result<String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| Error::other(e.to_string()))?;
    let sid = unsafe { token_user_sid(token) };
    drop(unsafe { CloseHandle(token) });
    sid
}

/// Read the user identifier out of an already open token. The token stays the
/// caller's to close, so every path here leaves it open.
#[expect(unsafe_code, reason = "reads a variable length token structure")]
unsafe fn token_user_sid(token: HANDLE) -> Result<String> {
    let mut needed = 0u32;
    drop(unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) });
    if needed == 0 {
        return Err(Error::other("the process token reports no user"));
    }
    // The structure holds pointers, so the buffer is word sized rather than
    // byte sized: a byte vector carries no alignment guarantee.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(|e| Error::other(e.to_string()))?;
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }
        .map_err(|e| Error::other(e.to_string()))?;
    let mut len = 0usize;
    while unsafe { *text.0.add(len) } != 0 {
        len += 1;
    }
    let units = unsafe { from_raw_parts(text.0, len) };
    let scope = String::from_utf16(units).map_err(|e| Error::other(e.to_string()));
    let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    scope
}

/// A nonce drawn from the system random source, which is seeded by the
/// platform and needs no state of its own.
#[expect(unsafe_code, reason = "draws from the system random source")]
pub(crate) fn system_random() -> Result<[u8; NONCE_LEN]> {
    let mut nonce = [0u8; NONCE_LEN];
    unsafe { BCryptGenRandom(None, &mut nonce, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .map_err(|e| Error::other(e.to_string()))?;
    Ok(nonce)
}
