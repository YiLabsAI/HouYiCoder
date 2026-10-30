//! Searching stored results without reading any of them whole.
//!
//! The scan walks one window at a time and stops at its budget. A window is
//! searched together with the tail of the one before it, so a match that
//! straddles two windows is found once, in the window where it ends; a hit
//! that lies wholly inside the carried tail was found in the earlier window
//! and is not reported twice. The tail is capped at half a window, which covers
//! a query of up to 8192 bytes; a longer query can miss a match that begins
//! before the carry. A scan that runs out of budget answers with the hits it
//! has and a cursor, never with "no match". A page always moves the cursor past
//! the position it was asked to continue from, so a caller that keeps asking
//! cannot be handed the same window twice.

use std::time::{Duration, Instant};

use houyicoder_api::result_catalog::ResultCatalogError;
use houyicoder_context::ContextBackend;
use houyicoder_protocol::result_catalog::{ByteSpan, ResultId, ScanBudget, SearchHit};

use crate::agent::model_window::span_for_lowercase_offsets;

use super::entry::BlockSource;
use super::windows::{DecodedWindow, WINDOW_BYTES, read_window};

/// The bytes one scan reads when the caller asks for no particular budget.
pub(super) const DEFAULT_SCAN_BYTES: u64 = 8 * 1024 * 1024;

/// The milliseconds one scan runs when the caller asks for no particular
/// budget.
pub(super) const DEFAULT_SCAN_MILLIS: u64 = 100;

/// The fewest bytes carried from one window into the next.
const MIN_CARRY: usize = 256;

/// How many characters of context a hit shows on each side.
const SNIPPET_CHARS: usize = 60;

/// One result a scan may read.
#[derive(Debug, Clone)]
pub(super) struct Target {
    /// The name the hit will answer with.
    pub id: ResultId,
    /// Where the bytes are.
    pub source: BlockSource,
}

/// What one scan found.
#[derive(Debug)]
pub(super) struct ScanResult {
    /// The hits, in scan order.
    pub hits: Vec<SearchHit>,
    /// Bytes the scan read from the store.
    pub scanned_bytes: u64,
    /// Whether the scan stopped before reaching the end.
    pub partial: bool,
    /// Where a scan that stopped continues: the target and its stored offset.
    pub next: Option<(usize, u64)>,
}

/// Search the targets from a position, reporting at most limit hits.
pub(super) async fn scan(
    backend: &dyn ContextBackend,
    query: &str,
    targets: &[Target],
    from_entry: usize,
    from_offset: u64,
    budget: ScanBudget,
    limit: usize,
) -> Result<ScanResult, ResultCatalogError> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Ok(ScanResult {
            hits: Vec::new(),
            scanned_bytes: 0,
            partial: false,
            next: None,
        });
    }
    // A carried tail long enough that any match ending in this window begins
    // inside it, with room for a lowercase form longer than the query.
    let carry_bytes = (needle.len() * 4).clamp(MIN_CARRY, WINDOW_BYTES / 2);
    let started = Instant::now();
    let mut hits = Vec::new();
    let mut scanned_bytes = 0u64;
    // Bytes this call read for the first time. The budget counts these only:
    // a walk toward the position the caller came from re-reads material an
    // earlier call already reported, and a scan that stopped inside it could
    // not advance the caller.
    let mut spent = 0u64;
    for (entry, target) in targets.iter().enumerate().skip(from_entry) {
        // A resumed scan starts one carry-length before the position it
        // continues from, so a match that begins there and ends after it is
        // seen in the window where it ends.
        let resume = if entry == from_entry { from_offset } else { 0 };
        let mut offset = resume.saturating_sub(carry_bytes as u64);
        // Everything at or before this stored offset was reported by an
        // earlier call or by an earlier window, so a match ending there is not
        // reported again.
        let mut covered_before = resume;
        let mut carry: Option<DecodedWindow> = None;
        let mut lines = Lines::new(offset == 0);
        loop {
            // A stop is only allowed where the cursor advances: stopping
            // short of the position the caller came with would answer with
            // the cursor it already has, and the caller would page the same
            // window forever.
            let advanced = entry != from_entry || offset > resume;
            if advanced
                && (spent >= budget.max_bytes
                    || started.elapsed() >= Duration::from_millis(budget.max_millis))
            {
                return Ok(ScanResult {
                    hits,
                    scanned_bytes,
                    partial: true,
                    next: Some((entry, offset)),
                });
            }
            let window = read_window(backend, &target.source, offset, WINDOW_BYTES).await?;
            if window.end <= offset {
                break;
            }
            scanned_bytes += window.end - window.start;
            spent += window.end.saturating_sub(resume.max(window.start));
            let view = match &carry {
                Some(previous) => previous.then(&window),
                None => window.clone(),
            };
            let map = lines.map(&view, carry.as_ref());
            let stopped = collect(
                &view,
                &map,
                &needle,
                covered_before,
                limit,
                target,
                &mut hits,
            );
            if let Some(at) = stopped {
                return Ok(ScanResult {
                    hits,
                    scanned_bytes,
                    partial: true,
                    next: Some((entry, at)),
                });
            }
            lines.advance(&window, carry_bytes);
            offset = window.end;
            covered_before = covered_before.max(window.end);
            carry = Some(window.tail(carry_bytes));
        }
    }
    Ok(ScanResult {
        hits,
        scanned_bytes,
        partial: false,
        next: None,
    })
}

/// Where the windows of one stream sit in its line numbering.
///
/// A scan that started partway through a stream has not seen the lines before
/// its start, so it reports no line numbers at all rather than a count that
/// begins in the middle of the stream.
struct Lines {
    /// Whether the scan has covered the stream from its start.
    known: bool,
    /// Newlines before the carried tail begins.
    carry_base: u64,
    /// Newlines before the window's own text begins.
    window_base: u64,
}

impl Lines {
    fn new(known: bool) -> Self {
        Self {
            known,
            carry_base: 0,
            window_base: 0,
        }
    }

    /// Where the decoded positions of one view sit in the stream.
    fn map<'a>(&self, view: &'a DecodedWindow, carry: Option<&DecodedWindow>) -> LineMap<'a> {
        LineMap {
            known: self.known,
            carry_base: self.carry_base,
            window_base: self.window_base,
            carry_len: carry.map_or(0, |tail| tail.text.len()),
            text: &view.text,
        }
    }

    /// Move the numbering past one window.
    fn advance(&mut self, window: &DecodedWindow, carry_bytes: usize) {
        let next_window = self.window_base + newlines(&window.text);
        let next_carry = window.tail(carry_bytes);
        self.carry_base = next_window.saturating_sub(newlines(&next_carry.text));
        self.window_base = next_window;
    }
}

/// The line numbering of one view: the carried tail and the window after it.
struct LineMap<'a> {
    /// Whether the numbering is known at all.
    known: bool,
    /// Newlines before the carried tail begins.
    carry_base: u64,
    /// Newlines before the window's own text begins.
    window_base: u64,
    /// Decoded length of the carried tail.
    carry_len: usize,
    /// The view's text.
    text: &'a str,
}

impl LineMap<'_> {
    /// The line a decoded index begins on, counting from one.
    fn line_at(&self, index: usize) -> Option<u64> {
        if !self.known {
            return None;
        }
        let (base, from, text) = if index < self.carry_len {
            (self.carry_base, index, &self.text[..self.carry_len])
        } else {
            let at = index - self.carry_len;
            (self.window_base, at, &self.text[self.carry_len..])
        };
        Some(base + newlines(&text[..from]) + 1)
    }
}

/// How many lines a piece of text holds.
fn newlines(text: &str) -> u64 {
    text.as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count() as u64
}

/// Append the hits one window holds, skipping the ones that end at or before
/// the boundary an earlier window already covered. Returns the stored offset a
/// scan that reached its hit limit continues from, so a page holds no more
/// hits than its ceiling and the rest of the window is not lost.
fn collect(
    view: &DecodedWindow,
    lines: &LineMap<'_>,
    needle: &str,
    covered_before: u64,
    limit: usize,
    target: &Target,
    hits: &mut Vec<SearchHit>,
) -> Option<u64> {
    let lower = view.text.to_lowercase();
    let mut cursor = 0;
    while cursor < lower.len() {
        let Some(found) = lower[cursor..].find(needle) else {
            break;
        };
        let at = cursor + found;
        let (start, end) = span_for_lowercase_offsets(&view.text, at, at + needle.len());
        let start_stored = view.stored_offset(start);
        let end_stored = view.stored_offset(end);
        if end_stored > covered_before {
            hits.push(SearchHit {
                id: target.id.clone(),
                span: ByteSpan {
                    start: start_stored,
                    len: end_stored.saturating_sub(start_stored),
                },
                line: lines.line_at(start),
                snippet: snippet(&view.text, start, end),
            });
            if hits.len() >= limit {
                return Some(end_stored);
            }
        }
        let next = (at + needle.len()).max(at + 1).min(lower.len());
        cursor = lower.ceil_char_boundary(next);
    }
    None
}

/// A short window of text around a hit, with ellipsis where the text was cut.
fn snippet(text: &str, start: usize, end: usize) -> String {
    let mut cut_start = start;
    for _ in 0..SNIPPET_CHARS {
        let Some(previous) = text[..cut_start].chars().next_back() else {
            break;
        };
        cut_start -= previous.len_utf8();
    }
    let mut cut_end = end;
    for _ in 0..SNIPPET_CHARS {
        let Some(next) = text[cut_end..].chars().next() else {
            break;
        };
        cut_end += next.len_utf8();
    }
    let mut out = String::new();
    if cut_start > 0 {
        out.push_str("...");
    }
    out.push_str(&text[cut_start..cut_end]);
    if cut_end < text.len() {
        out.push_str("...");
    }
    out
}
