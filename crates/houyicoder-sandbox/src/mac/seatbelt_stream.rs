//! stdout-streaming drain for the bash progress line count, split from
//! mac.rs so that file stays under the file-size gate. Pure: takes a
//! spawned Child (no MacSeatbeltSession dependency), so it is testable
//! without the seatbelt fence.

/// Stream a child's stdout + stderr to completion, counting stdout newlines
/// live into lines so the host's bash chip shows "(12s · 14 lines)".
/// Returns the same Output shape wait_with_output does, so the caller's
/// match is unchanged. Both pipes drain concurrently via tokio::join! —
/// reading stdout to EOF first would deadlock if the child fills stderr's
/// 64KB pipe buffer before stdout closes. The line count is the running
/// newline tally (0 before the first newline lands, incremented per chunk);
/// a command with no stdout never touches the counter (the caller's -1
/// sentinel stays, so the host shows no line count for it).
pub(super) async fn stream_drain(
    mut child: tokio::process::Child,
    lines: &std::sync::atomic::AtomicI64,
) -> std::io::Result<std::process::Output> {
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (stdout_res, stderr_res) = tokio::join!(
        async {
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 8192];
            let mut nl: i64 = 0;
            loop {
                let n = stdout.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                nl += chunk[..n].iter().filter(|b| **b == b'\n').count() as i64;
                lines.store(nl, Ordering::Relaxed);
                buf.extend_from_slice(&chunk[..n]);
            }
            Ok::<_, std::io::Error>(buf)
        },
        async {
            let mut buf = Vec::new();
            stderr.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        }
    );
    let stdout = stdout_res?;
    let stderr = stderr_res?;
    let status = child.wait().await?;
    Ok(std::process::Output {
        stdout,
        stderr,
        status,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::stream_drain;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// stream_drain counts stdout newlines live + returns the full output
    /// (same shape as wait_with_output). Spawns a real sh (no sandbox-exec —
    /// the drain itself is backend-agnostic), so it is fast + cross-platform.
    #[tokio::test]
    async fn test_drain_counts_lines() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf '1\\n2\\n3\\n4\\n5\\n'")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
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
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("true")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().expect("spawn sh");
        let counter = Arc::new(AtomicI64::new(-1));
        let output = stream_drain(child, &counter).await.expect("drain");
        assert_eq!(counter.load(Ordering::Relaxed), -1, "no stdout → sentinel");
        assert!(output.stdout.is_empty());
        assert!(output.status.success());
    }
}
