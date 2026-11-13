//! Decoding under each declared encoding, and the redaction a print must
//! keep.

use super::{SecretEncoding, SecretRead, decode_secret};

/// The same two bytes are a two-character key under one encoding and a
/// rejection under the other, which is what makes the declaration load
/// bearing rather than decorative.
#[test]
fn test_encoding_selects_decode() {
    assert_eq!(
        decode_secret(b"11", SecretEncoding::Utf8).as_deref(),
        Some("11")
    );
    assert_eq!(decode_secret(b"11", SecretEncoding::Utf16Le), None);
}

/// Text stored as little-endian code units decodes under that encoding.
#[test]
fn test_utf16_decodes_text() {
    let bytes: Vec<u8> = "sk-1".encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert_eq!(
        decode_secret(&bytes, SecretEncoding::Utf16Le).as_deref(),
        Some("sk-1")
    );
}

/// A lone trailing byte is not a code unit, so the value is rejected rather
/// than decoded one character short.
#[test]
fn test_odd_length_rejects() {
    assert_eq!(decode_secret(b"s\x00k", SecretEncoding::Utf16Le), None);
}

/// Bytes that are not UTF-8 text are rejected, not replaced: a replacement
/// character would be sent as part of the key.
#[test]
fn test_invalid_utf8_rejects() {
    assert_eq!(decode_secret(&[0xff, 0xfe], SecretEncoding::Utf8), None);
}

/// A value with surrounding whitespace is rejected. Trimming it would send a
/// key the store never held, and the failure would appear at the provider.
#[test]
fn test_padding_rejects() {
    assert_eq!(decode_secret(b" sk\n", SecretEncoding::Utf8), None);
}

/// An empty value is not a secret.
#[test]
fn test_empty_rejects() {
    assert_eq!(decode_secret(b"", SecretEncoding::Utf8), None);
}

/// A control character cannot travel in an http header value unescaped, so
/// the value that holds one is rejected.
#[test]
fn test_control_rejects() {
    assert_eq!(decode_secret(b"sk\x001", SecretEncoding::Utf8), None);
}

/// A print of the outcome never shows the value: a debug line that carried a
/// key would be a leaked key.
#[test]
fn test_debug_hides_value() {
    let read = SecretRead::Value("sk-secret".into());
    let printed = format!("{read:?}");
    assert!(
        !printed.contains("sk-secret"),
        "the value must not reach a log: {printed}"
    );
}

/// Every outcome still names itself in a print, so a diagnostic that shows
/// the debug form stays useful.
#[test]
fn test_debug_names_outcome() {
    assert_eq!(format!("{:?}", SecretRead::NotFound), "NotFound");
    assert_eq!(
        format!("{:?}", SecretRead::UnsupportedPlatform),
        "UnsupportedPlatform"
    );
    assert_eq!(format!("{:?}", SecretRead::InvalidValue), "InvalidValue");
    assert_eq!(format!("{:?}", SecretRead::Unavailable), "Unavailable");
}
