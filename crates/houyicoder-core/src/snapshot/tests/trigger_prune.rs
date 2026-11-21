//! Regression coverage for snapshot trigger accuracy, build-tree exclusion,
//! and retention bounds.

use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all, write};
use std::process::id;

use super::super::{MAX_UNDO_DEPTH, SnapshotStore, UndoEntry, is_destructive_command};

#[test]
fn test_rm_prefix_not_destructive() {
    for safe in [
        "cargo test -p rmcp",
        "cd $TMPDIR && mkdir rmcpchk && ls -la rmcpchk",
        "cargo test --workspace 2>&1",
        "curl -s https://crates.io/api/v1/crates/rmcp > /dev/null",
        "make verify > $TMPDIR/verify.log 2>&1",
        "make verify > ${TMPDIR}/verify.log 2>&1",
        "echo scratch > /tmp/probe.log",
        "rm -rf $TMPDIR/rmcpchk",
        "rm -f ${TMPDIR}/probe.rs /tmp/scratch.txt",
        "echo 'a > b'",
        "echo \"rm -rf src/\"",
        "echo \"escaped \\\"quote\\\" -> ok\" &> /dev/null",
        "python3 - <<'PY'\nrm -rf src/\nPY\n",
        "cat < Cargo.toml",
    ] {
        assert!(
            !is_destructive_command(safe),
            "read-only or scratch-only command must not trigger workspace snapshot: {safe}"
        );
    }

    for destructive in [
        "rm -rf src/",
        "/bin/rm Cargo.toml",
        "rmdir crates/old",
        "unlink Cargo.lock",
        "find . -name '*.bak' | xargs -0 rm",
        "FOO=1 env -i timeout -k 1 5 rm -rf src/",
        "cd $TMPDIR && rm -rf ../src",
        "echo \"rewritten\" > src/lib.rs",
        "echo err &>> build.log",
        "cat patch.txt >> Cargo.toml",
        "sudo apt install pkg",
        "git checkout -- src/lib.rs",
    ] {
        assert!(
            is_destructive_command(destructive),
            "workspace-modifying command must trigger snapshot: {destructive}"
        );
    }
}

#[test]
fn test_target_prefix_dirs_pruned() {
    let tmp = temp_dir().join(format!("snap-target-prefix-{}", id()));
    drop(remove_dir_all(&tmp));
    create_dir_all(tmp.join("src")).expect("src dir");
    create_dir_all(tmp.join("target-xwin").join("debug")).expect("target-xwin dir");
    create_dir_all(tmp.join("target").join("debug")).expect("target dir");
    write(tmp.join("src").join("lib.rs"), "pub fn ok() {}").expect("write src");
    write(
        tmp.join("target-xwin").join("debug").join("artifact.rlib"),
        "binary",
    )
    .expect("write target-xwin");
    write(
        tmp.join("target").join("debug").join("artifact.rlib"),
        "binary",
    )
    .expect("write target");

    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot { store_path, .. } = entry else {
        panic!("expected CoWSnapshot");
    };
    assert!(
        store_path.join("src").join("lib.rs").exists(),
        "source files are snapshotted"
    );
    assert!(
        !store_path.join("target").exists(),
        "default target directory is excluded"
    );
    assert!(
        !store_path.join("target-xwin").exists(),
        "prefixed build target directory is excluded from snapshots"
    );
    drop(remove_dir_all(&tmp));
}

#[test]
fn test_prune_caps_unprotected_count() {
    let tmp = temp_dir().join(format!("snap-count-cap-{}", id()));
    drop(remove_dir_all(&tmp));
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("a.txt"), "x").expect("write");

    let store = SnapshotStore::new(&tmp).expect("store");
    // Drop each returned entry immediately so none holds an active lease.
    for _ in 0..(MAX_UNDO_DEPTH + 5) {
        drop(store.snapshot(&[]).expect("snapshot"));
    }
    let removed = store.prune(604800, u64::MAX, &[]);
    assert_eq!(
        removed, 5,
        "unprotected unleased snapshots beyond the undo depth are pruned without waiting on TTL"
    );
    drop(remove_dir_all(&tmp));
}
