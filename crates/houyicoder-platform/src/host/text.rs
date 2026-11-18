//! Text decoding for output produced by a command interpreter.
//!
//! The interpreter declares the encoding; the decoder never guesses it from
//! the host console. UTF-8 and UTF-16 code units may split across reads, and a
//! legacy multibyte sequence may end halfway through a chunk, so streaming
//! decoding retains incomplete bytes until the next read completes them.

use std::mem::take;
use std::str::from_utf8;

use crate::host::shell::Shell;
#[cfg(target_os = "windows")]
use crate::windows::text::{decode_code_page, oem_code_page};

#[cfg(test)]
mod tests;

/// The byte encoding a command output stream declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputEncoding {
    /// Unicode UTF-8, including output negotiated by the command wrapper.
    #[default]
    Utf8,
    /// Little-endian UTF-16 code units.
    Utf16Le,
    /// A legacy Windows code page, selected only for a channel that declares it.
    LegacyCodePage(u16),
}

/// The encoding a shell's output streams use.
#[must_use]
pub fn output_encoding_for_shell(shell: Shell) -> OutputEncoding {
    match shell {
        Shell::Cmd => cmd_output_encoding(),
        Shell::Posix | Shell::GitBash | Shell::PowerShell(_) => OutputEncoding::Utf8,
    }
}

#[cfg(target_os = "windows")]
fn cmd_output_encoding() -> OutputEncoding {
    OutputEncoding::LegacyCodePage(oem_code_page())
}

#[cfg(not(target_os = "windows"))]
fn cmd_output_encoding() -> OutputEncoding {
    OutputEncoding::Utf8
}

/// Decode one complete byte buffer under its declared encoding.
#[must_use]
pub fn decode_complete(bytes: &[u8], encoding: OutputEncoding) -> String {
    match encoding {
        OutputEncoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        OutputEncoding::Utf16Le => decode_utf16_le(bytes, true),
        OutputEncoding::LegacyCodePage(code_page) => legacy_decode_lossy(bytes, code_page),
    }
}

/// What one complete buffer decoded to and how it was read.
///
/// A caller that persists the text also persists the encoding beside it. The
/// bytes alone do not say how they were read, so a reader of the stored form
/// replays this report rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedText {
    /// The text the bytes decoded to.
    pub text: String,
    /// The encoding the bytes were read under.
    pub encoding: OutputEncoding,
    /// Whether any sequence was invalid under that encoding and had to be
    /// replaced, so the text is not a faithful record of what was written.
    pub had_errors: bool,
}

/// Decode one complete byte buffer and report how it was read.
#[must_use]
pub fn decode_complete_report(bytes: &[u8], encoding: OutputEncoding) -> DecodedText {
    DecodedText {
        text: decode_complete(bytes, encoding),
        encoding,
        had_errors: has_invalid_sequences(bytes, encoding),
    }
}

/// Whether any byte sequence in the buffer is invalid under the encoding.
fn has_invalid_sequences(bytes: &[u8], encoding: OutputEncoding) -> bool {
    match encoding {
        OutputEncoding::Utf8 => from_utf8(bytes).is_err(),
        OutputEncoding::Utf16Le => {
            let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
            bytes.len() % 2 == 1 || utf16_units_have_unpaired_surrogate(bytes)
        }
        OutputEncoding::LegacyCodePage(code_page) => {
            legacy_decode_strict(bytes, code_page).is_none()
        }
    }
}

/// Whether the little-endian units pair every surrogate they open.
fn utf16_units_have_unpaired_surrogate(bytes: &[u8]) -> bool {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).is_err()
}

/// Decode the first character of a buffer and report how many bytes it
/// occupies.
///
/// None means the buffer holds only the beginning of a character, so a
/// window reader stops there and the next window decodes it whole. A byte
/// sequence that no character can start becomes a replacement mark and is
/// consumed, so one bad byte does not stall a read.
#[must_use]
pub fn next_char(bytes: &[u8], encoding: OutputEncoding) -> Option<(String, usize)> {
    match encoding {
        OutputEncoding::Utf8 => match from_utf8(bytes) {
            Ok(text) => first_char(text),
            Err(error) if error.valid_up_to() > 0 => {
                first_char(from_utf8(&bytes[..error.valid_up_to()]).ok()?)
            }
            Err(error) => error.error_len().map(|_| ("\u{fffd}".to_string(), 1)),
        },
        OutputEncoding::Utf16Le => next_utf16(bytes),
        OutputEncoding::LegacyCodePage(code_page) => next_code_page(bytes, code_page),
    }
}

/// The first character of decoded text, with its byte width.
fn first_char(text: &str) -> Option<(String, usize)> {
    text.chars()
        .next()
        .map(|ch| (ch.to_string(), ch.len_utf8()))
}

/// The first UTF-16 unit, joining a surrogate pair and holding back a high
/// surrogate whose partner was not read.
fn next_utf16(bytes: &[u8]) -> Option<(String, usize)> {
    if bytes.len() < 2 {
        return None;
    }
    let unit = u16::from_le_bytes([bytes[0], bytes[1]]);
    if (0xd800..=0xdbff).contains(&unit) {
        if bytes.len() < 4 {
            return None;
        }
        let low = u16::from_le_bytes([bytes[2], bytes[3]]);
        return match char::decode_utf16([unit, low]).next() {
            Some(Ok(ch)) => Some((ch.to_string(), 4)),
            _ => Some(("\u{fffd}".to_string(), 2)),
        };
    }
    if (0xdc00..=0xdfff).contains(&unit) {
        return Some(("\u{fffd}".to_string(), 2));
    }
    char::from_u32(u32::from(unit)).map(|ch| (ch.to_string(), 2))
}

/// The byte a window may resume decoding a double-byte code page at.
///
/// A double-byte code page's lead and trail ranges overlap, so counting back
/// a fixed number of bytes from a mid-character offset can land on a trail
/// byte and decode the rest of the window shifted by one. Every byte below
/// 0x40 is a single-byte character in these code pages and can never be a
/// trail byte, so the byte after the last one before the offset is a safe
/// starting point. The block start answers 0 when the window holds no such
/// byte.
#[must_use]
pub fn resync_offset(bytes: &[u8], up_to: usize) -> usize {
    let limit = up_to.min(bytes.len());
    bytes[..limit]
        .iter()
        .rposition(|byte| *byte < 0x40)
        .map_or(0, |at| at + 1)
}

/// The first character of a legacy code-page stream. A lead byte whose trail
/// has not arrived is held back; a byte pair that names no character is
/// replaced.
#[cfg(target_os = "windows")]
fn next_code_page(bytes: &[u8], code_page: u16) -> Option<(String, usize)> {
    bytes.first()?;
    if let Some(text) = legacy_decode_strict(&bytes[..1], code_page) {
        return Some((text, 1));
    }
    if bytes.len() < 2 {
        return None;
    }
    match legacy_decode_strict(&bytes[..2], code_page) {
        Some(text) => Some((text, 2)),
        None => Some(("\u{fffd}".to_string(), 1)),
    }
}

/// This host has no code-page table, so a stream recorded under one steps as
/// UTF-8, the same fallback a whole-buffer decode uses here.
#[cfg(not(target_os = "windows"))]
fn next_code_page(bytes: &[u8], _code_page: u16) -> Option<(String, usize)> {
    next_char(bytes, OutputEncoding::Utf8)
}

/// An incremental decoder for one command output stream.
///
/// stdout and stderr need separate instances: a partial code unit on one must
/// never be completed by bytes arriving on the other.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    encoding: OutputEncoding,
    pending: Vec<u8>,
    /// Whether the stream has delivered no complete unit yet. The leading
    /// byte order mark is removed only here: at any later position a byte
    /// pair that spells one is text the producer wrote.
    at_start: bool,
}

impl StreamDecoder {
    /// Start decoding one stream under its declared encoding.
    #[must_use]
    pub fn new(encoding: OutputEncoding) -> Self {
        Self {
            encoding,
            pending: Vec::new(),
            at_start: true,
        }
    }

    /// Add bytes and return every character that is complete so far.
    /// Incomplete trailing bytes remain buffered for the next call.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        match self.encoding {
            OutputEncoding::Utf8 => self.take_utf8(),
            OutputEncoding::Utf16Le => self.take_utf16(),
            OutputEncoding::LegacyCodePage(code_page) => self.take_legacy(code_page),
        }
    }

    /// Decode any bytes left at end of stream, replacing an incomplete unit.
    pub fn finish(&mut self) -> String {
        let rest = take(&mut self.pending);
        let text = match self.encoding {
            OutputEncoding::Utf16Le => decode_utf16_le(&rest, self.at_start),
            encoding => decode_complete(&rest, encoding),
        };
        self.at_start = self.at_start && rest.is_empty();
        text
    }

    fn take_utf8(&mut self) -> String {
        let mut output = String::new();
        let mut rest = take(&mut self.pending);
        loop {
            match from_utf8(&rest) {
                Ok(text) => {
                    output.push_str(text);
                    rest.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if let Ok(text) = from_utf8(&rest[..valid]) {
                        output.push_str(text);
                    }
                    rest.drain(..valid);
                    match error.error_len() {
                        Some(length) => {
                            output.push('\u{fffd}');
                            rest.drain(..length);
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending = rest;
        output
    }

    fn take_utf16(&mut self) -> String {
        let mut complete = self.pending.len() / 2 * 2;
        if complete >= 2 {
            let last = u16::from_le_bytes([self.pending[complete - 2], self.pending[complete - 1]]);
            if (0xd800..=0xdbff).contains(&last) {
                complete -= 2;
            }
        }
        let output = decode_utf16_le(&self.pending[..complete], self.at_start);
        self.pending.drain(..complete);
        if complete > 0 {
            self.at_start = false;
        }
        output
    }

    fn take_legacy(&mut self, code_page: u16) -> String {
        let (output, consumed) = legacy_prefix(
            &self.pending,
            |bytes| legacy_decode_strict(bytes, code_page),
            |bytes| legacy_decode_lossy(bytes, code_page),
        );
        self.pending.drain(..consumed);
        output
    }
}

/// Decode little-endian UTF-16 bytes, removing the leading byte order mark
/// only when the caller says the buffer is the start of the stream.
fn decode_utf16_le(bytes: &[u8], strip_bom: bool) -> String {
    let bytes = if strip_bom {
        bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes)
    } else {
        bytes
    };
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let mut output = String::from_utf16_lossy(&units);
    if bytes.len() % 2 == 1 {
        output.push('\u{fffd}');
    }
    output
}

/// Decode the longest valid prefix of a legacy stream. A short trailing
/// sequence that may be completed by the next chunk stays undecoded; an
/// invalid sequence elsewhere is replaced rather than blocking the stream.
fn legacy_prefix(
    bytes: &[u8],
    mut strict: impl FnMut(&[u8]) -> Option<String>,
    mut lossy: impl FnMut(&[u8]) -> String,
) -> (String, usize) {
    if let Some(text) = strict(bytes) {
        return (text, bytes.len());
    }
    for tail in 1..=bytes.len().min(4) {
        let split = bytes.len() - tail;
        if let Some(text) = strict(&bytes[..split]) {
            return (text, split);
        }
    }
    (lossy(bytes), bytes.len())
}

#[cfg(target_os = "windows")]
fn legacy_decode_strict(bytes: &[u8], code_page: u16) -> Option<String> {
    decode_code_page(bytes, code_page, true)
}

#[cfg(not(target_os = "windows"))]
fn legacy_decode_strict(_bytes: &[u8], _code_page: u16) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
fn legacy_decode_lossy(bytes: &[u8], code_page: u16) -> String {
    decode_code_page(bytes, code_page, false)
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(not(target_os = "windows"))]
fn legacy_decode_lossy(bytes: &[u8], code_page: u16) -> String {
    // The shell mapping selects legacy encodings only on Windows. A direct
    // caller on another host gets a visible diagnostic rather than a silent
    // claim that the bytes were decoded under this code page.
    tracing::warn!(
        code_page,
        "legacy output decoding is unavailable on this host"
    );
    String::from_utf8_lossy(bytes).into_owned()
}
