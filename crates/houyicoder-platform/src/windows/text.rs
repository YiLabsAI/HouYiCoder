//! Legacy code-page decoding through the Windows text conversion API.

#![expect(
    unsafe_code,
    reason = "the Windows text conversion API uses raw buffers"
)]

use windows::Win32::Globalization::{GetOEMCP, MB_ERR_INVALID_CHARS, MultiByteToWideChar};

/// The OEM code page used by the native command interpreter.
pub(crate) fn oem_code_page() -> u16 {
    u16::try_from(unsafe { GetOEMCP() }).unwrap_or(0)
}

/// Convert bytes from a declared legacy code page to UTF-16, returning None
/// when strict conversion rejects the input or the code page is unavailable.
pub(crate) fn decode_code_page(bytes: &[u8], code_page: u16, strict: bool) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    let flags = if strict {
        MB_ERR_INVALID_CHARS
    } else {
        Default::default()
    };
    let required = unsafe { MultiByteToWideChar(code_page.into(), flags, bytes, None) };
    if required <= 0 {
        return None;
    }
    let mut wide = vec![0u16; required as usize];
    let written = unsafe { MultiByteToWideChar(code_page.into(), flags, bytes, Some(&mut wide)) };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    Some(String::from_utf16_lossy(&wide))
}

#[cfg(test)]
mod tests;
