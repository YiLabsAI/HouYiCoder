//! Tests for this module.

use super::*;
use crate::launcher::{SpawnPolicy, SpawnRequest};
use std::env::temp_dir;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::process::{Command, id};
use std::time::Duration;
use std::time::Instant;
#[cfg(unix)]
use tokio::time::sleep;

#[tokio::test]
async fn test_sync_output_captures_stdout() {
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("echo")
        .with_args(["hello"])
        .piped_output();
    let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    let exit = child.wait().await.unwrap();
    assert_eq!(exit.exit_code, Some(0));
    assert_eq!(exit.stdout.as_deref(), Some("hello\n"));
}

#[tokio::test]
async fn test_sync_output_nonzero_exit() {
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("sh")
        .with_args(["-c", "exit 3"])
        .piped_output();
    let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    let exit = child.wait().await.unwrap();
    assert_eq!(exit.exit_code, Some(3));
}

#[tokio::test]
async fn test_sync_output_stderr() {
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("sh")
        .with_args(["-c", "echo err 1>&2"])
        .piped_output();
    let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    let exit = child.wait().await.unwrap();
    assert!(exit.stderr.as_deref().is_some_and(|s| s.contains("err")));
}

#[tokio::test]
async fn test_workspace_as_cwd() {
    let launcher = StdProcessLauncher::new();
    let tmp = temp_dir();
    // pwd (Git Bash on Windows) translates the Windows path to a Unix
    // form (/tmp for the temp dir), so the basename check fails. Use
    // cmd /c cd on Windows to get the native path; pwd on Unix.
    #[cfg(windows)]
    let (program, args): (&str, Vec<&str>) = ("cmd", vec!["/c", "cd"]);
    #[cfg(not(windows))]
    let (program, args): (&str, Vec<&str>) = ("pwd", vec![]);
    let req = SpawnRequest::new(program)
        .with_args(args)
        .with_workspace(tmp.clone())
        .piped_output();
    let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    let exit = child.wait().await.unwrap();
    let out = exit.stdout.unwrap_or_default();
    let basename = tmp
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert!(
        out.contains(&basename),
        "cwd output should contain the temp dir basename, got: {out}"
    );
}

/// An interactive spawn hands back live stdin/stdout pipe handles the
/// caller drives directly. Echo a line through a cat child to prove the
/// pipes are wired both ways.
#[tokio::test]
async fn test_interactive_hands_back_pipes() {
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("cat").interactive();
    let mut child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    let pipes = child.take_pipes().expect("interactive spawn pipes back");
    let mut stdin = pipes.stdin.expect("stdin piped");
    let stdout = pipes.stdout.expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    stdin.write_all(b"ping\n").unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "ping\n");
    drop(stdin);
    drop(reader);
    // Dropping the child triggers the on-drop kill so the test process
    // does not leak a cat.
    drop(child.wait().await);
}

/// The capture path honors the fence wall timeout. Without it a child that
/// never exits blocks the caller forever: the capture path is the one a
/// caller reading a secret off stdout must use, so an unbounded wait there
/// means a hung helper stalls whatever asked for the secret.
#[tokio::test]
async fn test_capture_honors_wall_timeout() {
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("sleep").with_args(["30"]).piped_output();
    let policy = SpawnPolicy::default().with_fence(FenceConfig {
        wall_timeout_ms: 300,
        ..FenceConfig::default()
    });
    let start = Instant::now();
    let child = launcher.spawn(req, policy).expect("spawn succeeds");
    let outcome = child.wait().await;
    let elapsed = start.elapsed();
    assert!(
        outcome.is_err(),
        "a child outliving the wall timeout must not report success"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the wait must be bounded by the timeout, not the child's lifetime; \
         took {elapsed:?}"
    );
}

/// A capture spawn with no fence still waits for the child, so the
/// bounded-wait path does not truncate a slow-but-finishing command.
#[tokio::test]
async fn test_capture_without_fence_waits() {
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("sh")
        .with_args(["-c", "sleep 0.3; echo late"])
        .piped_output();
    let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    let exit = child.wait().await.unwrap();
    assert_eq!(exit.exit_code, Some(0));
    assert_eq!(exit.stdout.as_deref(), Some("late\n"));
}

/// Output that exceeds the pipe buffer still comes back whole: the drain
/// runs concurrently with the wait, so a child writing more than the pipe
/// capacity cannot deadlock against a waiter holding an undrained pipe.
#[tokio::test]
async fn test_capture_survives_large_output() {
    let launcher = StdProcessLauncher::new();
    // 400 KiB, well past the typical 64 KiB pipe buffer.
    let req = SpawnRequest::new("sh")
        .with_args(["-c", "yes abcdefghij | head -c 409600"])
        .piped_output();
    let policy = SpawnPolicy::default().with_fence(FenceConfig {
        wall_timeout_ms: 20_000,
        ..FenceConfig::default()
    });
    let child = launcher.spawn(req, policy).unwrap();
    let exit = child.wait().await.unwrap();
    assert_eq!(exit.stdout.map(|s| s.len()), Some(409_600));
}

/// A non-interactive spawn leaves pipes None so the take_pipes contract is
/// unambiguous.
#[tokio::test]
async fn test_capture_spawn_no_pipes() {
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("true").piped_output();
    let mut child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    assert!(child.take_pipes().is_none());
    drop(child.wait().await);
}

/// A spawn with no piped output runs on the async path and still ends.
///
/// That path owns the child through a future rather than a drain, so dropping
/// the future is how a caller abandons the command.
#[tokio::test]
async fn test_async_spawn_ends() {
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("true");
    let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    let exit = child.wait().await.expect("the spawn reports its exit");
    assert_eq!(exit.exit_code, Some(0));
}

/// Whether a process whose command line carries the marker is still in the
/// table. The marker is a shell variable, so it exists only in the grandchild's
/// command line and not in the shell that started it.
#[cfg(unix)]
fn marker_alive(marker: &str) -> bool {
    !marker_rows(marker).is_empty()
}

/// The process id and group of every live process carrying the marker.
#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "test process table inspection, not an engine spawn"
)]
fn marker_rows(marker: &str) -> Vec<(i32, i32)> {
    let listed = Command::new("ps")
        .args(["-ax", "-o", "pid=,pgid=,command="])
        .output()
        .expect("list processes");
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter(|line| line.contains(marker) && line.contains("sleep"))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let pgid = parts.next()?.parse().ok()?;
            Some((pid, pgid))
        })
        .collect()
}

/// Dropping a spawn before it is awaited kills the work a wrapper forked, not
/// only the wrapper. A kill of the direct child alone would leave that work
/// running under init.
#[cfg(unix)]
#[tokio::test]
async fn test_drop_kills_forked_grandchild() {
    let marker = format!("houyi-drop-{}", id());
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("sh").with_args(["-c", &format!("m={marker}; sleep 60")]);
    let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    let started = Instant::now();
    while !marker_alive(&marker) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the grandchild never appeared"
        );
        sleep(Duration::from_millis(20)).await;
    }
    drop(child);
    let started = Instant::now();
    while marker_alive(&marker) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "dropping the spawn left the forked work running"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// A command that exits on its own may leave work running. Killing its group
/// after a successful wait would end that work, so a spawn that was awaited
/// must not. The leftover starts its own group: a shell reaps a background job
/// that stays in its group when it exits, so only a group kill from this
/// launcher can explain the leftover dying.
#[cfg(unix)]
#[tokio::test]
async fn test_awaited_spawn_spares_group() {
    let marker = format!("houyi-kept-{}", id());
    let launcher = StdProcessLauncher::new();
    // A shell reaps a background job that stays in its group, so that death
    // would not tell a shell reap apart from a group kill. The leftover starts
    // its own group; only a group kill from this launcher can then explain it
    // dying.
    let req = SpawnRequest::new("sh").with_args([
        "-c",
        &format!(
            "perl -e 'setpgrp(0,0); open STDIN, \"</dev/null\"; open STDOUT, \">/dev/null\"; open STDERR, \">&STDOUT\"; $0 = \"sleep 60 {marker}\"; sleep 60' & echo started"
        ),
    ]);
    let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
    let command_group = child.id.expect("the wrapper has a pid") as i32;
    let started = Instant::now();
    let left = loop {
        let rows = marker_rows(&marker);
        if rows.iter().any(|(_, pgid)| *pgid != command_group) {
            break true;
        }
        if started.elapsed() > Duration::from_secs(5) {
            break false;
        }
        sleep(Duration::from_millis(20)).await;
    };
    assert!(left, "the leftover never left the command group");
    let exit = child.wait().await.expect("the wrapper exits");
    assert_eq!(exit.exit_code, Some(0));
    sleep(Duration::from_millis(200)).await;
    let alive = marker_alive(&marker);
    if alive {
        #[expect(
            clippy::disallowed_methods,
            reason = "test cleanup of a process this test started"
        )]
        drop(Command::new("pkill").args(["-f", &marker]).status());
    }
    assert!(
        alive,
        "a finished spawn must not kill work that left its group"
    );
}
