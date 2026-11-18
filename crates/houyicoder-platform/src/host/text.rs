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
        OutputEncoding::Utf16Le => decode_utf16_lossy(bytes),
        OutputEncoding::LegacyCodePage(code_page) => legacy_decode_lossy(bytes, code_page),
    }
}

/// An incremental decoder for one command output stream.
///
/// stdout and stderr need separate instances: a partial code unit on one must
/// never be completed by bytes arriving on the other.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    encoding: OutputEncoding,
    pending: Vec<u8>,
}

impl StreamDecoder {
    /// Start decoding one stream under its declared encoding.
    #[must_use]
    pub fn new(encoding: OutputEncoding) -> Self {
        Self {
            encoding,
            pending: Vec::new(),
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
        decode_complete(&take(&mut self.pending), self.encoding)
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
        let output = decode_complete(&self.pending[..complete], OutputEncoding::Utf16Le);
        self.pending.drain(..complete);
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

fn decode_utf16_lossy(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
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
