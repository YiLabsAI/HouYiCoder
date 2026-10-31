//! Tests for the shared byte-size formatter.

use super::format_byte_size;

/// Sizes below a kibibyte stay whole bytes, and a size that would print as
/// 1024.0 in the smaller unit promotes to the next one.
#[test]
fn test_size_binary_units() {
    assert_eq!(format_byte_size(0), "0 B");
    assert_eq!(format_byte_size(1023), "1023 B");
    assert_eq!(format_byte_size(1024), "1.0 KiB");
    assert_eq!(format_byte_size(1024 * 1024), "1.0 MiB");
    assert_eq!(format_byte_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    // One byte short of a mebibyte rounds to 1024.0 KiB, so it promotes
    // rather than print a number the unit cannot hold.
    assert_eq!(format_byte_size(1_048_575), "1.0 MiB");
    assert_eq!(format_byte_size(1024 * 1024 * 1024 - 1), "1.0 GiB");
}
