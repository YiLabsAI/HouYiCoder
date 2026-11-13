//! Reading a stored secret from the system store.
//!
//! The bytes a store returns are decoded under one declared encoding and
//! accepted only as a whole valid value, so a wrong guess is reported rather
//! than repaired. A decoder that trimmed a stray byte would hand back a key
//! that fails at the provider with no hint about which store produced it.

use std::fmt;
use std::str::from_utf8;

#[cfg(test)]
mod tests;

/// How the bytes of a stored secret are encoded.
///
/// There is deliberately no default variant. A store that fell back to some
/// encoding would hand back mojibake as a key, and the failure would surface
/// far from the store that caused it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretEncoding {
    /// Text encoded as UTF-8.
    Utf8,
    /// Little-endian UTF-16 code units, the encoding the Windows credential
    /// store uses for a stored password.
    Utf16Le,
}

/// What a read of the system store produced.
#[derive(PartialEq, Eq)]
pub enum SecretRead {
    /// The stored value, decoded under the encoding the store declares.
    Value(String),
    /// The store holds no item for this service and account.
    NotFound,
    /// The item exists but holds bytes that are not a valid secret.
    InvalidValue,
    /// This host has no system secret store.
    UnsupportedPlatform,
    /// The store exists but could not be queried.
    Unavailable,
}

/// Written by hand so the value never reaches a log through a derived print:
/// a secret that is debug-printed has leaked.
impl fmt::Debug for SecretRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Value(_) => "Value(<redacted>)",
            Self::NotFound => "NotFound",
            Self::InvalidValue => "InvalidValue",
            Self::UnsupportedPlatform => "UnsupportedPlatform",
            Self::Unavailable => "Unavailable",
        };
        f.write_str(label)
    }
}

/// Decode stored bytes under the declared encoding.
///
/// Returns None unless every character is ASCII graphic, which is all an http
/// header value can carry without an encoder escaping it. A space anywhere in
/// the value is rejected, not just at the ends. Bytes that are not valid under
/// the declared encoding, an empty value, and any padding are rejected rather
/// than repaired.
pub fn decode_secret(bytes: &[u8], encoding: SecretEncoding) -> Option<String> {
    let text = match encoding {
        SecretEncoding::Utf8 => from_utf8(bytes).ok()?.to_owned(),
        SecretEncoding::Utf16Le => decode_utf16(bytes)?,
    };
    is_secret_shaped(&text).then_some(text)
}

/// Little-endian code units, paired before decoding so a lone trailing byte
/// is a rejection rather than a truncated character.
fn decode_utf16(bytes: &[u8]) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// A secret is one non-empty run of printable ASCII.
fn is_secret_shaped(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_graphic())
}

#[cfg(target_os = "macos")]
use crate::macos::read_secret as read_platform_secret;
#[cfg(target_os = "windows")]
use crate::windows::read_secret as read_platform_secret;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_platform_secret(_service: &str, _account: &str) -> SecretRead {
    SecretRead::UnsupportedPlatform
}

/// Read the secret the system store holds for this service and account.
///
/// Each store declares its own encoding; the caller states neither, because
/// the convention belongs to the store. A host with no store answers
/// UnsupportedPlatform so the caller reports that rather than failing
/// quietly.
pub fn read_secret(service: &str, account: &str) -> SecretRead {
    read_platform_secret(service, account)
}
