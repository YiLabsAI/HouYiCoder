//! The log as line-aligned byte windows, read forward by offset and backward
//! from the newest bytes.
//!
//! A window edge can fall inside a line, so the bytes either side of the
//! window are consulted to learn whether its end segments are lines the log
//! shows whole. The bytes split only on the newline byte, which is ASCII, so
//! a multi-byte character never meets a split inside it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::str::from_utf8;
use std::sync::atomic::Ordering;

use houyicoder_context::{ContextError, LogRangeRead, MAX_WINDOW_BYTES, ReverseRead, SessionId};

use super::LocalFileBackend;

/// Whether the log's byte at the given offset ends a line. A read holds the
/// bytes of its own window, so this answers for the one byte either side of it:
/// the byte before the window's first says whether the window starts at a
/// line's head, the byte at its newest boundary whether the window's last
/// stretch is a line the log terminates. A read that fails leaves the caller
/// without the fact, which costs it the segment rather than the whole batch.
fn byte_ends_line(f: &mut File, at: u64) -> bool {
    let mut one = [0u8; 1];
    f.seek(SeekFrom::Start(at)).is_ok() && f.read_exact(&mut one).is_ok() && one[0] == b'\n'
}

/// The bytes one backward read covers, oldest first, and what the log holds
/// either side of them. Splitting these bytes into lines is all the read needs:
/// an edge can fall anywhere inside a line, so the pieces are put together from
/// the window rather than from a carry that has to survive the walk.
struct LogWindow<'a> {
    bytes: &'a [u8],
    /// Absolute offset of the window's first byte.
    start: u64,
    /// The read's newest boundary: one past the window's last byte.
    from_byte: u64,
    /// The window holds the log from its first byte.
    covers_log: bool,
    /// The byte before the window ends a line, so the window's first segment
    /// has a head the log shows.
    starts_at_line: bool,
    /// The byte at from_byte ends a line, so the window's last segment is a
    /// line the log terminates without the window holding the terminator.
    ends_with_line: bool,
}

/// What a window's bytes yield: the whole lines and whether the newest whole
/// line the log holds at or below the window came back.
struct WindowLines {
    /// (byte offset of the line's start, raw line text), newest first.
    lines: Vec<(u64, String)>,
    /// True when the batch's first line is the log's newest whole line at or
    /// below the window. False when the window yields no line, or yields lines
    /// older than that one: its budget ran out on bytes holding no terminator
    /// before reaching the line, so no read of that budget returns it.
    newest_line_returned: bool,
}

impl LogWindow<'_> {
    /// The whole lines the window holds, newest first. A segment whose head or
    /// terminator the window does not show is no line yet: the walk re-reads it
    /// from a boundary that holds it, or, for a line wider than the read's
    /// budget, never.
    fn whole_lines(&self) -> WindowLines {
        let mut lines: Vec<(u64, String)> = Vec::new();
        let mut head_shown = self.starts_at_line;
        let mut start = 0usize;
        // One past the newest returned line's terminator, or past the line's
        // own end when the log holds no terminator for it. At the window's
        // start, so a window that yields no line leaves every byte of itself
        // to answer whether a terminator sits above the batch.
        let mut newest_end = self.start;
        for (i, &b) in self.bytes.iter().enumerate() {
            if b != b'\n' {
                continue;
            }
            if head_shown {
                let text = String::from_utf8_lossy(&self.bytes[start..i]).into_owned();
                newest_end = self.start + i as u64 + 1;
                lines.push((self.start + start as u64, text));
            }
            start = i + 1;
            head_shown = true;
        }
        // The bytes after the last terminator in the window: a whole line when
        // the log terminates it at the window's boundary, when they are the
        // whole log, which one line written without a terminator ends as, or
        // when the window ends on the terminator itself, which ends an empty
        // line. An empty window holds no line at all.
        if !self.bytes.is_empty() {
            let one_line_log = self.covers_log && start == 0;
            if self.ends_with_line || one_line_log {
                let text = String::from_utf8_lossy(&self.bytes[start..]).into_owned();
                newest_end = self.from_byte;
                lines.push((self.start + start as u64, text));
            }
        }
        lines.reverse();
        // A terminator among the bytes above the newest returned line ends a
        // newer line the window does not hold whole, so no read of this window
        // returns it. A window that yields no line returns none: it holds no
        // head or no terminator to end one, so a caller has nothing to take for
        // the log's newest line.
        let above = (newest_end - self.start) as usize;
        let newest_line_returned =
            !lines.is_empty() && !self.bytes[above.min(self.bytes.len())..].contains(&b'\n');
        WindowLines {
            newest_line_returned,
            lines,
        }
    }
}

impl LocalFileBackend {
    /// Forward line-aligned window: seek to byte_offset, skip a partial
    /// first line (if byte_offset lands mid-line) to the next newline, then
    /// read complete lines until max_bytes consumed. UTF-8 safe -- the split
    /// is on the newline byte, a single-byte ASCII, never inside a multi-byte
    /// sequence.
    pub(super) fn read_byte_range(
        &self,
        session: SessionId,
        byte_offset: u64,
        max_bytes: u64,
    ) -> LogRangeRead {
        let log = self.log_path(session);
        let Ok(f) = File::open(&log) else {
            return LogRangeRead::default();
        };
        let mut f = f;
        let bytes_total = f.metadata().map(|m| m.len()).unwrap_or(0);
        if byte_offset >= bytes_total {
            return LogRangeRead {
                bytes_total,
                ..Default::default()
            };
        }
        if f.seek(SeekFrom::Start(byte_offset)).is_err() {
            return LogRangeRead {
                bytes_total,
                ..Default::default()
            };
        }
        let mut buf = vec![0u8; max_bytes.min(MAX_WINDOW_BYTES) as usize];
        let n = f.read(&mut buf).unwrap_or(0);
        buf.truncate(n);
        // Skip a partial first line when byte_offset lands mid-line (the byte
        // before it is not a newline). When byte_offset is a line start --
        // which is the case for every window after the first when the caller
        // advances by next_offset (a line-aligned offset) -- the first
        // segment is a complete line and must NOT be skipped. Reading the
        // byte at offset-1 distinguishes the two; a seek-and-read of one
        // byte is cheap (one syscall per window).
        let mut start = 0usize;
        if byte_offset != 0 {
            let mid_line = if n == 0 {
                true
            } else {
                let prev_byte = f.seek(SeekFrom::Start(byte_offset - 1)).ok().and_then(|_| {
                    let mut b = [0u8; 1];
                    f.read(&mut b).ok().filter(|&r| r == 1).map(|_| b[0])
                });
                // Restore the read cursor so the line-split below sees the
                // window from byte_offset. If the seek-back failed, fall back
                // to the conservative mid-line skip.
                drop(f.seek(SeekFrom::Start(byte_offset)));
                prev_byte != Some(b'\n')
            };
            if mid_line {
                if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    start = nl + 1;
                } else {
                    // No newline in the window -- the whole window is one
                    // partial line; advance past it so the next window starts
                    // clean.
                    return LogRangeRead {
                        next_offset: byte_offset + n as u64,
                        bytes_total,
                        ..Default::default()
                    };
                }
            }
        }
        // Split the complete lines (start..n), tracking byte offsets. One
        // terminator per returned line, plus the one the mid-line skip passed
        // over: together they are every terminator in the span the read
        // consumed. Validate the span once: a log written here is valid UTF-8,
        // and the read can stop inside a character whose tail is never
        // returned. Genuinely invalid bytes still fall back to the per-line
        // conversion, which keeps the damaged line readable.
        let complete_end = buf[start..]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|last| start + last + 1);
        let valid_window = complete_end.and_then(|end| from_utf8(&buf[start..end]).ok());
        let mut lines = Vec::new();
        let mut line_breaks = if start > 0 { 1 } else { 0 };
        let mut cur = start;
        let mut line_start_abs = byte_offset + start as u64;
        while cur < buf.len() {
            let nl = buf[cur..].iter().position(|&b| b == b'\n');
            let line_end = match nl {
                Some(p) => cur + p,
                None => break, // last partial line -- drop, the next window reads it whole
            };
            let text = match valid_window {
                Some(window) => window[cur - start..line_end - start].to_owned(),
                None => {
                    self.lossy_line_count.fetch_add(1, Ordering::Relaxed);
                    String::from_utf8_lossy(&buf[cur..line_end]).into_owned()
                }
            };
            lines.push((line_start_abs, text));
            line_breaks += 1;
            line_start_abs += (line_end - cur + 1) as u64; // +1 for the \n
            cur = line_end + 1;
        }
        let next_offset = if lines.is_empty() {
            byte_offset + n as u64
        } else {
            line_start_abs
        };
        LogRangeRead {
            lines,
            next_offset,
            bytes_total,
            line_breaks,
        }
    }

    /// Backward whole-line read: take the newest max_bytes of the log as one
    /// window and split it into lines. A window edge can fall inside a line,
    /// so the byte either side of the window is read to learn whether the
    /// segments at its ends are lines the log shows whole. The bytes stay
    /// raw, so a multi-byte UTF-8 sequence never meets a split inside it.
    pub(super) fn read_reverse_lines(
        &self,
        session: SessionId,
        from_byte: u64,
        max_bytes: u64,
    ) -> Result<ReverseRead, ContextError> {
        let log = self.log_path(session);
        // An unopenable log is an error, not empty: empty reads as the log's
        // end and would latch the caller's view closed.
        let mut f = File::open(&log).map_err(|_| ContextError::Io)?;
        let size = f
            .metadata()
            .map(|m| m.len())
            .map_err(|_| ContextError::Io)?;
        if size == 0 {
            return Ok(ReverseRead::default());
        }
        let from_byte = from_byte.min(size);
        // One byte at the least, so the window's start sits below the read's
        // boundary and a walk over the log always moves down.
        let start = from_byte.saturating_sub(max_bytes.max(1));
        let mut bytes = vec![0u8; (from_byte - start) as usize];
        f.seek(SeekFrom::Start(start))
            .map_err(|_| ContextError::Io)?;
        f.read_exact(&mut bytes).map_err(|_| ContextError::Io)?;
        let window = LogWindow {
            bytes: &bytes,
            start,
            from_byte,
            covers_log: start == 0 && from_byte == size,
            starts_at_line: start == 0 || byte_ends_line(&mut f, start - 1),
            ends_with_line: from_byte < size && byte_ends_line(&mut f, from_byte),
        };
        let whole = window.whole_lines();
        // Where a walk resumes. A window starting at a line's head left nothing
        // over, so the next read ends there. One starting inside a line holds
        // that line's tail only: the next read must end just past its
        // terminator to hold it whole, or, for a line wider than the read, pass
        // it. The walk always moves down, which is what ends a walk over a log
        // whose lines are wider than the read.
        let next_from = if start == 0 {
            None
        } else if window.starts_at_line {
            Some(start)
        } else {
            let past_line = bytes
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| start + i as u64 + 1)
                .filter(|end| *end < from_byte);
            Some(past_line.unwrap_or(start))
        };
        Ok(ReverseRead {
            lines: whole.lines,
            newest_line_returned: whole.newest_line_returned,
            next_from,
        })
    }
}
