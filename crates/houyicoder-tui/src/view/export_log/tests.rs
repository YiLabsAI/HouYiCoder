use super::*;
use std::env::temp_dir;
use std::fs::metadata;
use std::fs::read;
use std::fs::remove_file;
use std::os::unix::fs::PermissionsExt;

#[test]
fn test_write_round_trips_bytes() {
    let dir = temp_dir();
    let path = dir.join("houyi_export_test_roundtrip.json");
    remove_file(&path).ok();
    let payload = b"{\"hello\":\"world\"}";
    write_atomic_0600(&path, payload).expect("write");
    let read = read(&path).expect("read");
    assert_eq!(read, payload);
    #[cfg(unix)]
    {
        let mode = metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "export file must be 0o600 (owner-only)"
        );
    }
    remove_file(&path).ok();
}

#[test]
fn test_write_atomic_replaces_existing() {
    // A second write to the same path replaces the first content; the
    // temp-then-rename path must not leave the old bytes visible.
    let dir = temp_dir();
    let path = dir.join("houyi_export_test_replace.json");
    remove_file(&path).ok();
    write_atomic_0600(&path, b"first").expect("write 1");
    write_atomic_0600(&path, b"second").expect("write 2");
    let read = read(&path).expect("read");
    assert_eq!(read, b"second");
    remove_file(&path).ok();
}
