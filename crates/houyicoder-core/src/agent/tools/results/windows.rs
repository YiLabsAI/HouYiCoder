//! Bounded reads of a stored result, decoded to text where the bytes are
//! text.
//!
//! A stored stream is one of three shapes: the producer's bytes untouched, a
//! JSON string value, or JSON text. A window can land in the middle of a
//! character or an escape, so every decode reports the stored span it actually
//! covered and the next read resumes there. A window never asks the store for
//! more than WINDOW_BYTES, which keeps a page of a very large result bounded.

use std::str::from_utf8;

use houyicoder_api::result_catalog::ResultCatalogError;
use houyicoder_context::{BlockHash, ContextBackend, ContextError};

use super::entry::{BlockSource, StoredShape};

/// The most stored bytes one window read asks for.
pub(super) const WINDOW_BYTES: usize = 64 * 1024;

/// How many bytes a window reaches back before its offset, so a character the
/// offset splits can still be decoded whole.
const LOOK_BACK: u64 = 3;

/// One decoded window: the text it covers and where that text sits in the
/// stored block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DecodedWindow {
    /// The text the window covers.
    pub text: String,
    /// Stored offset of the first decoded byte.
    pub start: u64,
    /// Stored offset where decoding stopped, which is where the next window
    /// begins.
    pub end: u64,
    /// Where the stored and decoded offsets diverge: the decoded index and the
    /// stored offset of the same position. The first entry is always the start
    /// of the window.
    breaks: Vec<(usize, u64)>,
    /// The stored length of the whole block.
    pub total: u64,
}

impl DecodedWindow {
    /// A window with no text at all, for a read that landed past the end.
    pub(super) fn empty(total: u64) -> Self {
        Self {
            text: String::new(),
            start: total,
            end: total,
            breaks: vec![(0, total)],
            total,
        }
    }

    /// The stored offset the decoded byte at this index came from.
    pub(super) fn stored_offset(&self, decoded: usize) -> u64 {
        let mut stored = self.start;
        for &(at, offset) in &self.breaks {
            if at > decoded {
                break;
            }
            stored = offset + (decoded - at) as u64;
        }
        stored
    }

    /// The first of this window's stored bytes that a reader asked for, cut
    /// at a character boundary so a resumed read never starts inside one.
    /// Always covers at least one character, so a continuation advances.
    pub(super) fn take_bytes(&self, want: u64) -> Self {
        let want = want.max(1);
        if self.end.saturating_sub(self.start) <= want {
            return self.clone();
        }
        let limit = self.start.saturating_add(want);
        let mut cut = 0;
        for (at, ch) in self.text.char_indices() {
            let end = at + ch.len_utf8();
            if cut > 0 && self.stored_offset(end) > limit {
                break;
            }
            cut = end;
        }
        let stored = self.stored_offset(cut);
        let mut breaks = vec![(0, self.start)];
        breaks.extend(
            self.breaks
                .iter()
                .filter(|(at, _)| *at > 0 && *at < cut)
                .map(|(at, offset)| (*at, *offset)),
        );
        Self {
            text: self.text[..cut].to_string(),
            start: self.start,
            end: stored,
            breaks,
            total: self.total,
        }
    }

    /// The last bytes of this window, as a window of their own.
    pub(super) fn tail(&self, bytes: usize) -> Self {
        if self.text.len() <= bytes {
            return self.clone();
        }
        let mut cut = self.text.len() - bytes;
        while cut < self.text.len() && !self.text.is_char_boundary(cut) {
            cut += 1;
        }
        let stored = self.stored_offset(cut);
        let mut breaks = vec![(0, stored)];
        breaks.extend(
            self.breaks
                .iter()
                .filter(|(at, _)| *at >= cut)
                .map(|(at, offset)| (at - cut, *offset)),
        );
        Self {
            text: self.text[cut..].to_string(),
            start: stored,
            end: self.end,
            breaks,
            total: self.total,
        }
    }

    /// This window followed by another, as one window. The second must begin
    /// where this one stopped.
    pub(super) fn then(&self, next: &Self) -> Self {
        let offset = self.text.len();
        let mut breaks = self.breaks.clone();
        breaks.extend(
            next.breaks
                .iter()
                .map(|(at, stored)| (at + offset, *stored)),
        );
        Self {
            text: format!("{}{}", self.text, next.text),
            start: self.start,
            end: next.end,
            breaks,
            total: next.total,
        }
    }
}

/// Read one window of a stored result.
pub(super) async fn read_window(
    backend: &dyn ContextBackend,
    source: &BlockSource,
    offset: u64,
    want: usize,
) -> Result<DecodedWindow, ResultCatalogError> {
    let want = want.clamp(1, WINDOW_BYTES);
    // An escaped value has no character a stored offset can split, and its
    // escapes have to be walked from a boundary anyway.
    let look_back = match source.shape {
        StoredShape::JsonString => 0,
        StoredShape::Raw | StoredShape::JsonText => offset.min(LOOK_BACK),
    };
    let base = offset - look_back;
    let len = want
        .saturating_add(look_back as usize)
        .saturating_add(LOOK_BACK as usize)
        .min(WINDOW_BYTES);
    let window = backend
        .block_get_range(&source.hash, base, len)
        .await
        .map_err(|error| read_error(&source.hash, error))?;
    let total = window.total;
    if offset >= total {
        return Ok(DecodedWindow::empty(total));
    }
    match source.shape {
        StoredShape::JsonString => {
            decode_json_string(backend, &source.hash, &window.bytes, base, offset, total).await
        }
        StoredShape::Raw | StoredShape::JsonText => {
            Ok(decode_text(&window.bytes, base, offset, total))
        }
    }
}

/// The most bytes one escape occupies: the six of a code unit, doubled for
/// the surrogate pair that spells one character above the basic plane.
const ESCAPE_BYTES: usize = 12;

/// What the first byte of a stored block says about how to read it, with the
/// block's stored length.
pub(super) async fn peek(
    backend: &dyn ContextBackend,
    hash: &BlockHash,
) -> Result<(StoredShape, u64), ResultCatalogError> {
    let window = backend
        .block_get_range(hash, 0, 1)
        .await
        .map_err(|error| read_error(hash, error))?;
    let shape = match window.bytes.first() {
        Some(b'"') => StoredShape::JsonString,
        _ => StoredShape::JsonText,
    };
    Ok((shape, window.total))
}

/// Decode a window of bytes that are text as stored.
///
/// Bytes that are not text are skipped, and each skip leaves a break: without
/// one, every character after an undecodable byte would answer with a stored
/// offset short by the bytes skipped before it, so a search hit or a resumed
/// read would name a position the text does not start at.
fn decode_text(bytes: &[u8], base: u64, offset: u64, total: u64) -> DecodedWindow {
    let mut from = usize::try_from(offset - base).unwrap_or(0).min(bytes.len());
    // An offset inside a character still names the character the reader asked
    // for, so the window reaches back to where that character begins rather
    // than treating its own first bytes as undecodable.
    while from > 0 && bytes.get(from).is_some_and(|byte| byte & 0xC0 == 0x80) {
        from -= 1;
    }
    let mut text = String::new();
    let mut breaks = vec![(0usize, base + from as u64)];
    let mut index = from;
    while index < bytes.len() {
        match from_utf8(&bytes[index..]) {
            Ok(rest) => {
                text.push_str(rest);
                index = bytes.len();
            }
            Err(error) if error.valid_up_to() > 0 => {
                let end = index + error.valid_up_to();
                text.push_str(from_utf8(&bytes[index..end]).unwrap_or_default());
                index = end;
                // The window edge cut a character; the next read starts there
                // and decodes it whole.
                break;
            }
            Err(error) => {
                let Some(skip) = error.error_len() else {
                    break;
                };
                index += skip.max(1);
                breaks.push((text.len(), base + index as u64));
            }
        }
    }
    DecodedWindow {
        text,
        start: base + from as u64,
        end: base + index as u64,
        breaks,
        total,
    }
}

/// Decode a window of one JSON string value, escaping undone.
///
/// Every escape leaves two breaks: one where the escape starts and one where
/// the stored bytes resume after it. Without the second, each decoded
/// character after an escape would map to a stored offset short by the
/// escape's overhead, and a search hit or a cursor landing after one would
/// name the wrong byte.
async fn decode_json_string(
    backend: &dyn ContextBackend,
    hash: &BlockHash,
    bytes: &[u8],
    base: u64,
    offset: u64,
    total: u64,
) -> Result<DecodedWindow, ResultCatalogError> {
    let mut index = usize::try_from(offset - base).unwrap_or(0).min(bytes.len());
    if offset == 0 && bytes.first() == Some(&b'"') {
        index = 1;
    }
    let start = index;
    if bytes.get(index) == Some(&b'"') {
        // This offset holds the quote that closes the value, so the value has
        // no text left to read and there is nothing to report as undecodable.
        return Ok(DecodedWindow::empty(total));
    }
    let mut text = String::new();
    let mut breaks = vec![(0usize, base + index as u64)];
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                // The closing quote ends the value. The window ends past it so
                // a reader parked on the quote advances instead of reading the
                // same empty window again.
                index += 1;
                break;
            }
            b'\\' => {
                let (raw, taken) = match escape_len(bytes, index) {
                    Some(len) => (bytes[index..index + len].to_vec(), len),
                    None => {
                        // The window cut the escape short. Read it on its own;
                        // stopping here would resume at the same offset and
                        // stop there again.
                        let at = base + index as u64;
                        let extra = backend
                            .block_get_range(hash, at, ESCAPE_BYTES)
                            .await
                            .map_err(|error| read_error(hash, error))?;
                        let Some(len) = escape_len(&extra.bytes, 0) else {
                            return Err(ResultCatalogError::Corrupt(format!(
                                "the string at byte {at} is not well formed"
                            )));
                        };
                        (extra.bytes[..len].to_vec(), len)
                    }
                };
                breaks.push((text.len(), base + index as u64));
                push_escape(&mut text, &raw);
                index += taken;
                breaks.push((text.len(), base + index as u64));
            }
            _ => {
                let mut run_end = index;
                while run_end < bytes.len() && bytes[run_end] != b'"' && bytes[run_end] != b'\\' {
                    run_end += 1;
                }
                let (run, begun, ended) = text_at(&bytes[..run_end], index);
                if ended <= begun {
                    break;
                }
                text.push_str(&run);
                index = ended;
                // Bytes the run could not decode mean the block is not the
                // string it claims to be; the read stops after the bytes it
                // did cover.
                if ended < run_end {
                    break;
                }
            }
        }
    }
    Ok(DecodedWindow {
        text,
        start: base + start as u64,
        end: base + index as u64,
        breaks,
        total,
    })
}

/// The longest run of valid text at or after this index, with where it starts
/// and stops. A partial character at either edge is left for the next window.
///
/// The reported start is the index the window asked for, not the first byte
/// that decoded: bytes the run had to skip are still bytes the window
/// covered, and a caller that reported only the decoded part would leave the
/// next read at the skipped byte with nothing to advance it.
fn text_at(bytes: &[u8], from: usize) -> (String, usize, usize) {
    let mut start = from;
    while start < bytes.len() {
        match from_utf8(&bytes[start..]) {
            Ok(text) => return (text.to_string(), from, bytes.len()),
            Err(error) if error.valid_up_to() > 0 => {
                let end = start + error.valid_up_to();
                let text = from_utf8(&bytes[start..end])
                    .unwrap_or_default()
                    .to_string();
                return (text, from, end);
            }
            Err(error) => start += error.error_len().unwrap_or(1).max(1),
        }
    }
    (String::new(), from, bytes.len())
}

/// How many bytes the escape at this index takes, if the window holds all of
/// them.
fn escape_len(bytes: &[u8], at: usize) -> Option<usize> {
    match *bytes.get(at + 1)? {
        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Some(2),
        b'u' => {
            let code = hex4(bytes, at + 2)?;
            if (0xD800..0xDC00).contains(&code) {
                if bytes.get(at + 6..at + 8) != Some(b"\\u") {
                    return None;
                }
                hex4(bytes, at + 8)?;
                Some(12)
            } else {
                Some(6)
            }
        }
        _ => None,
    }
}

/// The code unit four hex digits spell.
fn hex4(bytes: &[u8], at: usize) -> Option<u16> {
    let raw = bytes.get(at..at + 4)?;
    u16::from_str_radix(from_utf8(raw).ok()?, 16).ok()
}

/// Append one escape's decoded character.
fn push_escape(text: &mut String, raw: &[u8]) {
    match raw.get(1) {
        Some(b'"') => text.push('"'),
        Some(b'\\') => text.push('\\'),
        Some(b'/') => text.push('/'),
        Some(b'b') => text.push('\u{8}'),
        Some(b'f') => text.push('\u{c}'),
        Some(b'n') => text.push('\n'),
        Some(b'r') => text.push('\r'),
        Some(b't') => text.push('\t'),
        Some(b'u') => text.push(unicode_char(raw)),
        _ => {}
    }
}

/// The character one unicode escape spells. A lone surrogate has no character
/// of its own, so it decodes to the replacement character rather than
/// dropping the byte the producer wrote.
fn unicode_char(raw: &[u8]) -> char {
    let Some(high) = hex4(raw, 2) else {
        return char::REPLACEMENT_CHARACTER;
    };
    if (0xD800..0xDC00).contains(&high) {
        let low = hex4(raw, 8).unwrap_or(0);
        if (0xDC00..0xE000).contains(&low) {
            let code = 0x1_0000 + ((high as u32 - 0xD800) << 10) + (low as u32 - 0xDC00);
            return char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER);
        }
        return char::REPLACEMENT_CHARACTER;
    }
    char::from_u32(high as u32).unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// Map a store failure onto the answer the caller understands. The block is
/// named in the answer because the store reports a failure without it, and a
/// reader told only that some read failed cannot tell which stream is gone.
pub(super) fn read_error(hash: &BlockHash, error: ContextError) -> ResultCatalogError {
    match error {
        ContextError::NotFound => ResultCatalogError::Corrupt(format!("block {} is gone", hash.0)),
        ContextError::Unsupported => ResultCatalogError::UnsupportedRead(format!(
            "this store cannot read a window of block {}",
            hash.0
        )),
        ContextError::Corrupt(detail) => ResultCatalogError::Corrupt(detail),
        ContextError::Io => {
            ResultCatalogError::Store(format!("block {} could not be read", hash.0))
        }
    }
}
