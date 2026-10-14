//! Tests for this module.

use super::*;

/// A mutator that, on its first call, writes a different file to the path
/// to simulate a concurrent writer racing between our CAS check and rename.
/// This forces the verify check to see content != our json, triggering the
/// retry path.
#[test]
fn test_cas_retries_toctou_race() {
    let dir = temp_dir().join(format!("cas-toctou-{}-{}", id(), line!()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    write(&path, r#"{"v":0}"#).unwrap();
    let first = AtomicBool::new(true);
    let path_for_mutator = path.clone();
    let result = update_settings(
        &path,
        |v| {
            if first.swap(false, Ordering::SeqCst) {
                // Simulate a concurrent writer: modify the file so our CAS
                // token is stale, forcing the verify to see different
                // content and retry.
                write(&path_for_mutator, r#"{"v":99}"#).unwrap();
            }
            v["v"] = 1.into();
        },
        5,
    );
    assert!(result.is_ok(), "CAS retry should succeed: {result:?}");
    let after: Value = serde_json::from_str(&read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after["v"], 1, "our mutation landed after retry");
    drop(remove_dir_all(&dir));
}
