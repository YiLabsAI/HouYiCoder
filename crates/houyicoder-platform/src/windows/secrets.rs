//! Reads a stored secret from the Windows credential store.
//!
//! The store returns the password as a byte blob with no encoding field, so
//! the encoding the store writes is declared here once and every read is
//! decoded under it. The account is checked against the stored user name so a
//! read for one account never returns another's password.

use std::ffi::c_void;
use std::iter::once;
use std::ptr::null_mut;
use std::slice::from_raw_parts;

use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW};
use windows::core::{HRESULT, PCWSTR};

use crate::host::secrets::{SecretEncoding, SecretRead, decode_secret};

/// The encoding the credential store writes for a stored password.
const STORE_ENCODING: SecretEncoding = SecretEncoding::Utf16Le;

#[expect(
    unsafe_code,
    reason = "reads a stored credential through the system API"
)]
pub(crate) fn read_secret(service: &str, account: &str) -> SecretRead {
    // A credential read is the event an operator should be able to see: it
    // names the item asked for, never the value it holds.
    tracing::info!(service, account, "reading credential store item");
    let target = wide(service);
    let mut raw: *mut CREDENTIALW = null_mut();
    match unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut raw) } {
        Ok(()) => {}
        Err(error) if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => {
            tracing::warn!(service, account, "credential item missing or access denied");
            return SecretRead::NotFound;
        }
        Err(_) => {
            tracing::warn!(service, account, "credential read failed");
            return SecretRead::Unavailable;
        }
    }
    let held = HeldCredential(raw);
    // The lookup resolves by service alone, so the account is what tells this
    // entry from another stored under the same service.
    if held.user_name() != account {
        return SecretRead::NotFound;
    }
    match decode_secret(held.blob(), STORE_ENCODING) {
        Some(key) => SecretRead::Value(key),
        None => SecretRead::InvalidValue,
    }
}

/// Frees the credential on every path out, including an early return.
struct HeldCredential(*mut CREDENTIALW);

impl HeldCredential {
    /// The user name the entry was stored under.
    #[expect(unsafe_code, reason = "reads a field of the returned record")]
    fn user_name(&self) -> String {
        let name = unsafe { (*self.0).UserName };
        if name.is_null() {
            return String::new();
        }
        unsafe { name.to_string() }.unwrap_or_default()
    }

    /// The stored password bytes. An entry with no blob is an empty slice
    /// rather than a slice built from a null pointer.
    #[expect(unsafe_code, reason = "reads the blob the store returned")]
    fn blob(&self) -> &[u8] {
        let credential = unsafe { &*self.0 };
        if credential.CredentialBlob.is_null() || credential.CredentialBlobSize == 0 {
            return &[];
        }
        unsafe {
            from_raw_parts(
                credential.CredentialBlob,
                credential.CredentialBlobSize as usize,
            )
        }
    }
}

impl Drop for HeldCredential {
    #[expect(unsafe_code, reason = "releases the record the store allocated")]
    fn drop(&mut self) {
        unsafe { CredFree(self.0 as *const c_void) };
    }
}

/// A string as the null-terminated wide text the API takes.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(once(0)).collect()
}
