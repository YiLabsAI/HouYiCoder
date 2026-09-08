use super::*;

#[test]
fn test_read_max_bytes_zero() {
    let err = validate_read_max_bytes(0).unwrap_err();
    assert!(err.to_string().contains("max_bytes"));
}

#[test]
fn test_read_max_bytes_positive() {
    assert!(validate_read_max_bytes(1).is_ok());
    assert!(validate_read_max_bytes(65_536).is_ok());
}
