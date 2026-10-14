//! Tests for this module.

use super::*;

// Known SHA-256 vector: sha256("abc") = ba7816bf... (FIPS 180-2 test 2).
#[test]
fn test_sha256_hex_known_vector() {
    let BlockHash(hex) = sha256_hex(b"abc");
    assert_eq!(
        hex,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn test_sha256_hex_empty_input() {
    let BlockHash(hex) = sha256_hex(b"");
    assert_eq!(
        hex,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(hex.len(), 64);
}
