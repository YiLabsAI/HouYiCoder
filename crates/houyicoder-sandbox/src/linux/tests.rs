// Construction probes the fence binary but never fences this process, so
// these tests run on any Linux host regardless of kernel support: the
// probe answer depends on the kernel, the resolver, grant and coverage
// behavior do not.

use super::*;
use std::env::temp_dir;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::process::id;
use std::process::{ExitStatus, Output};
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn unique_dir(tag: &str) -> PathBuf {
    temp_dir().join(format!(
        "houyicoder-linux-{tag}-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

#[test]
fn test_session_constructs_workspace() {
    let session = LinuxLandlockSession::new().expect("session");
    assert!(session.workspace.exists());
    assert!(session.workspace.is_dir());
    assert!(session.tmpdir.is_dir());
}

#[test]
fn test_absolute_path_rejected() {
    let session = LinuxLandlockSession::new().expect("session");
    let resolved = session.resolve("/etc/passwd");
    assert!(matches!(resolved, Err(SandboxError::PathTraversal(_))));
}

#[test]
fn test_granted_dir_admitted() {
    let session = LinuxLandlockSession::new().expect("session");
    let granted = unique_dir("grant");
    fs::create_dir_all(&granted).expect("create granted dir");
    let target = granted.join("notes.txt");
    fs::write(&target, "x").expect("write target");
    let granted_str = granted.to_str().expect("granted str");
    let target_str = target.to_str().expect("target str");

    session.add_working_dir(granted_str).expect("grant");
    let resolved = session.resolve_write(target_str).expect("admitted");
    let canonical_granted = dunce::canonicalize(&granted).expect("canonical granted");
    assert!(resolved.starts_with(canonical_granted));

    session.remove_working_dir(granted_str);
    let revoked = session.resolve_write(target_str);
    assert!(matches!(revoked, Err(SandboxError::PathTraversal(_))));

    let _cleanup = fs::remove_dir_all(&granted);
}

#[test]
fn test_scratch_dir_admitted() {
    let session = LinuxLandlockSession::new().expect("session");
    let target = session.tmpdir.join("scratch.txt");
    let target_str = target.to_str().expect("target str");
    let resolved = session.resolve_write(target_str).expect("scratch admitted");
    assert!(resolved.starts_with(&session.tmpdir));
}

#[test]
fn test_coverage_lists_grants() {
    let mut session = LinuxLandlockSession::new().expect("session");
    session.fence = FenceStatus::Enforced;
    let granted = unique_dir("coverage");
    fs::create_dir_all(&granted).expect("create granted dir");
    let granted_str = granted.to_str().expect("granted str");
    session.add_working_dir(granted_str).expect("grant");

    match session.coverage() {
        Coverage::Fenced { writable_roots } => {
            assert!(
                writable_roots.iter().any(|r| r == &session.workspace),
                "the workspace must be a fenced root: {writable_roots:?}"
            );
            let canonical = dunce::canonicalize(&granted).expect("canonical granted");
            assert!(
                writable_roots.contains(&canonical),
                "every write grant must be a fenced root: {writable_roots:?}"
            );
            assert!(
                !writable_roots.contains(&session.tmpdir),
                "the scratch dir is not a user-authorized root: {writable_roots:?}"
            );
        }
        other => panic!("an enforced fence must report fenced coverage: {other:?}"),
    }

    session.remove_working_dir(granted_str);
    match session.coverage() {
        Coverage::Fenced { writable_roots } => {
            assert_eq!(writable_roots, vec![session.workspace.clone()]);
        }
        other => panic!("revoking a grant must not unfence: {other:?}"),
    }
    let _cleanup = fs::remove_dir_all(&granted);
}

#[test]
fn test_coverage_unfenced() {
    let mut session = LinuxLandlockSession::new().expect("session");
    session.fence = FenceStatus::Failed("probe".into());
    assert!(matches!(session.coverage(), Coverage::Unfenced));
    session.fence = FenceStatus::NotEnforced;
    assert!(matches!(session.coverage(), Coverage::Unfenced));
}

/// The argv is the kernel grant set: the workspace and scratch dir come
/// first as write grants, runtime grants follow in flag pairs by kind,
/// and the command sits alone after the separator.
#[test]
fn test_fence_argv_grants() {
    let session = LinuxLandlockSession::new().expect("session");
    let write_dir = unique_dir("argv-write");
    fs::create_dir_all(&write_dir).expect("create write dir");
    let read_dir = unique_dir("argv-read");
    fs::create_dir_all(&read_dir).expect("create read dir");
    session
        .add_working_dir(write_dir.to_str().expect("write str"))
        .expect("grant write");
    session
        .add_reading_dir(read_dir.to_str().expect("read str"))
        .expect("grant read");

    let argv = session.fence_argv("touch x");
    let canonical_write = dunce::canonicalize(&write_dir).expect("canonical write");
    let canonical_read = dunce::canonicalize(&read_dir).expect("canonical read");
    let expected: Vec<OsString> = vec![
        "--write".into(),
        session.workspace.as_os_str().to_os_string(),
        "--write".into(),
        session.tmpdir.as_os_str().to_os_string(),
        "--write".into(),
        canonical_write.into_os_string(),
        "--read".into(),
        canonical_read.into_os_string(),
        "--".into(),
        "touch x".into(),
    ];
    assert_eq!(argv, expected, "argv must carry the full grant set");

    let _cleanup = fs::remove_dir_all(&write_dir);
    let _cleanup = fs::remove_dir_all(&read_dir);
}

/// Every word of the fence binary protocol, including the ones a healthy host
/// never sees: an unrecognized word must land in Failed carrying the
/// reason, never be mistaken for a working fence.
#[cfg(feature = "enforce")]
#[test]
fn test_probe_word_mapping() {
    assert!(matches!(
        fence_from_probe("enforced\n"),
        FenceStatus::Enforced
    ));
    assert!(matches!(
        fence_from_probe("enforced-partial"),
        FenceStatus::Enforced
    ));
    assert!(matches!(
        fence_from_probe("not-enforced"),
        FenceStatus::NotEnforced
    ));
    assert!(matches!(
        fence_from_probe("unavailable"),
        FenceStatus::Unavailable
    ));
    match fence_from_probe("failed:abi too old") {
        FenceStatus::Failed(reason) => assert_eq!(reason, "abi too old"),
        other => panic!("a failed probe must carry its reason: {other:?}"),
    }
    match fence_from_probe("garbage") {
        FenceStatus::Failed(reason) => assert_eq!(reason, "garbage"),
        other => panic!("an unknown word must land in Failed: {other:?}"),
    }
}

/// The stream and exit-code mapping every caller sees. The raw wait
/// status carries the exit code in its upper bits; a signal death would
/// report None instead, which the Option in the answer preserves.
#[test]
fn test_output_maps_result() {
    let output = Output {
        status: ExitStatus::from_raw(3 << 8),
        stdout: b"out".to_vec(),
        stderr: b"err".to_vec(),
    };
    let result = exec_result_from_output(output);
    assert_eq!(result.stdout, "out");
    assert_eq!(result.stderr, "err");
    assert_eq!(result.exit_code, Some(3));
}

/// The fence binary the daemon looks for must be the artifact this crate
/// builds: the name is a file name joined onto the daemon's directory, so a
/// mismatch degrades every command to an unfenced run with only an audit
/// line. Pin the constant to the file on disk so a rename cannot drift.
#[test]
fn test_fence_bin_matches_artifact() {
    let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/bin")
        .join(format!("{FENCE_BIN}.rs"));
    assert!(
        bin.is_file(),
        "FENCE_BIN must name a binary in src/bin: {bin:?} is missing"
    );
}
