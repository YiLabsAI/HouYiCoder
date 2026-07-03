//! Integration tests for the merge-preserving, CAS-guarded settings write.
//!
//! These exercise the real filesystem (a per-test temp dir) — the layer the
//! inline unit tests cannot reach without mutating shared state.

use std::path::PathBuf;

use houyicoder_config::update_settings;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("houyi-settings-{}-{tag}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_write_preserves_unknown_keys() {
    let dir = temp_dir("preserves");
    let path = dir.join("settings.json");
    let pre = r#"{
        "auto_memory": false,
        "auto_dream": true,
        "sandbox": {"network": {"mode": "off"}},
        "model": {"catalog": [{"id": "qwen3.7-max"}]},
        "foo": "unknown-key-must-survive"
    }"#;
    std::fs::write(&path, pre).unwrap();

    update_settings(
        &path,
        |v| {
            v["auto_memory"] = true.into();
        },
        3,
    )
    .unwrap();

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after["auto_memory"], true, "mutator target key written");
    assert_eq!(after["auto_dream"], true, "unrelated toggle key preserved");
    assert_eq!(
        after["sandbox"]["network"]["mode"], "off",
        "sandbox.network preserved"
    );
    assert_eq!(
        after["model"]["catalog"][0]["id"], "qwen3.7-max",
        "model.catalog preserved"
    );
    assert_eq!(
        after["foo"], "unknown-key-must-survive",
        "unknown key preserved"
    );
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn test_corrupt_json_not_overwritten() {
    let dir = temp_dir("corrupt");
    let path = dir.join("settings.json");
    let broken = "{ this is not valid json";
    std::fs::write(&path, broken).unwrap();

    let result = update_settings(
        &path,
        |v| {
            v["auto_memory"] = true.into();
        },
        3,
    );

    assert!(
        result.is_err(),
        "corrupt JSON must not be silently overwritten"
    );
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, broken, "broken file left exactly as-is");
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn test_write_creates_new_file() {
    let dir = temp_dir("new");
    let path = dir.join("settings.json");
    assert!(!path.exists());

    update_settings(
        &path,
        |v| {
            v["auto_memory"] = true.into();
        },
        3,
    )
    .unwrap();

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after["auto_memory"], true, "target key written to new file");
    assert!(
        after.get("sandbox").map(|v| v.is_null()).unwrap_or(true),
        "no sandbox injected into a fresh file"
    );
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn test_concurrent_write_keeps_keys() {
    let dir = temp_dir("concurrent");
    let path = dir.join("settings.json");
    std::fs::write(
        &path,
        r#"{"auto_memory":false,"auto_dream":false,"sandbox":{"network":{"mode":"off"}},"foo":"keep"}"#,
    )
    .unwrap();

    let p1 = path.clone();
    let t1 = std::thread::spawn(move || {
        update_settings(
            &p1,
            |v| {
                v["auto_memory"] = true.into();
            },
            10,
        )
    });
    let p2 = path.clone();
    let t2 = std::thread::spawn(move || {
        update_settings(
            &p2,
            |v| {
                v["auto_dream"] = true.into();
            },
            10,
        )
    });
    drop(t1.join().unwrap());
    drop(t2.join().unwrap());

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        after["sandbox"]["network"]["mode"], "off",
        "sandbox not lost to a concurrent writer"
    );
    assert_eq!(after["foo"], "keep", "unknown key not lost");
    assert_eq!(after["auto_memory"], true, "both writes land via CAS retry");
    assert_eq!(after["auto_dream"], true, "both writes land via CAS retry");
    drop(std::fs::remove_dir_all(&dir));
}
