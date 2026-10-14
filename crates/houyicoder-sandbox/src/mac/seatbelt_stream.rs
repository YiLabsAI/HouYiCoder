//! stdout-streaming drain for the bash progress line count, split from
//! mac.rs so that file stays under the file-size gate. Pure: takes a
//! spawned Child (no MacSeatbeltSession dependency), so it is testable
//! without the seatbelt fence.

use std::io::Error;
use std::process::Output;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use tokio::io::AsyncReadExt;
use tokio::process::Child;

/// Stream a child's stdout + stderr to completion, counting stdout newlines
/// live into lines so the host's bash chip shows "(12s · 14 lines)".
/// Returns the same Output shape wait_with_output does, so the caller's
/// match is unchanged. Both pipes drain concurrently via tokio::join! —
/// reading stdout to EOF first would deadlock if the child fills stderr's
/// 64KB pipe buffer before stdout closes. The line count is the running
/// newline tally (0 before the first newline lands, incremented per chunk);
/// a command with no stdout never touches the counter (the caller's -1
/// sentinel stays, so the host shows no line count for it).
pub(super) async fn stream_drain(mut child: Child, lines: &AtomicI64) -> std::io::Result<Output> {
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
            Ok::<_, Error>(buf)
        },
        async {
            let mut buf = Vec::new();
            stderr.read_to_end(&mut buf).await?;
            Ok::<_, Error>(buf)
        }
    );
    let stdout = stdout_res?;
    let stderr = stderr_res?;
    let status = child.wait().await?;
    Ok(Output {
        stdout,
        stderr,
        status,
    })
}

#[cfg(test)]
mod tests;
