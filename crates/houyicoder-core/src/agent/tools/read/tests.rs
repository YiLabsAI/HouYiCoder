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

#[test]
fn test_slice_read_window_offsets() {
    let text = "line1\nline2\nline3\nline4\nline5\n";
    let (sliced, truncated) = slice_read_window(text, false, Some(2), Some(2), 65_536);
    assert_eq!(sliced, "line2\nline3\n");
    assert!(truncated, "lines remain after the requested window");

    let (tail, tail_trunc) = slice_read_window(text, false, Some(4), Some(5), 65_536);
    assert_eq!(tail, "line4\nline5\n");
    assert!(!tail_trunc, "window reached end of file");

    let (byte_capped, cap_trunc) = slice_read_window(text, false, Some(2), None, 6);
    assert_eq!(byte_capped, "line2\n");
    assert!(cap_trunc, "byte ceiling truncated the slice");
}

#[test]
fn test_parse_line_param_forms() {
    let input = json!({"offset": "300", "limit": 40, "bad": "abc"});
    assert_eq!(parse_line_param(&input, "offset").unwrap(), Some(300));
    assert_eq!(parse_line_param(&input, "limit").unwrap(), Some(40));
    assert_eq!(parse_line_param(&input, "missing").unwrap(), None);
    assert!(parse_line_param(&input, "bad").is_err());
    assert!(validate_read_limit(Some(0)).is_err());
    assert!(validate_read_limit(Some(10)).is_ok());
}
