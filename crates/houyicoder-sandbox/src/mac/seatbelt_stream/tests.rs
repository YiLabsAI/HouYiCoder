#![allow(clippy::disallowed_methods)]
use super::stream_drain;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::process::Command;

/// stream_drain counts stdout newlines live + returns the full output
/// (same shape as wait_with_output). Spawns a real sh (no sandbox-exec —
/// the drain itself is backend-agnostic), so it is fast + cross-platform.
#[tokio::test]
async fn test_drain_counts_lines() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("printf '1\\n2\\n3\\n4\\n5\\n'")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn sh");
    let counter = Arc::new(AtomicI64::new(-1));
    let output = stream_drain(child, &counter).await.expect("drain");
    assert_eq!(counter.load(Ordering::Relaxed), 5, "final line count");
    assert_eq!(output.stdout, b"1\n2\n3\n4\n5\n");
    assert!(output.status.success(), "sh exited clean");
}

/// A command with no stdout leaves the counter at the -1 sentinel (the
/// host shows no line count for it); stderr still drains + the exit
/// status still lands.
#[tokio::test]
async fn test_drain_no_stdout_sentinel() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn sh");
    let counter = Arc::new(AtomicI64::new(-1));
    let output = stream_drain(child, &counter).await.expect("drain");
    assert_eq!(counter.load(Ordering::Relaxed), -1, "no stdout → sentinel");
    assert!(output.stdout.is_empty());
    assert!(output.status.success());
}
