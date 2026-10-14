use super::*;

#[test]
fn test_skip_unchanged_write() {
    let content = b"hello";
    // Flag off -> never skip, even when content matches.
    assert!(!should_skip_unchanged_write(false, Some(content), content));
    // Flag on but no existing file -> create it, no skip.
    assert!(!should_skip_unchanged_write(true, None, content));
    // Flag on, existing differs -> write.
    assert!(!should_skip_unchanged_write(true, Some(b"world"), content));
    // Flag on, existing equals content -> skip.
    assert!(should_skip_unchanged_write(true, Some(content), content));
}
