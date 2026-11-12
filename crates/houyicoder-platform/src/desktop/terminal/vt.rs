//! A small virtual-terminal byte decoder for a terminal whose input arrives
//! as a byte stream rather than as console records. A terminal window that
//! hands the process a pipe answers no console API, so the control keys and
//! the escape sequences the stream carries have to be decoded here.
//!
//! The decoder is incremental: bytes arrive in arbitrary chunks, so an
//! escape sequence may split across them, and a caller feeds one chunk at a
//! time and takes the events each chunk completed.

use std::io::Read;
use std::io::stdin;
use std::mem::take;
use std::str::from_utf8;

/// One decoded input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VtEvent {
    /// A control key: the letter the control block maps to.
    Ctrl(char),
    /// A plain character, with no modifier held.
    Char(char),
    /// The enter key.
    Enter,
    /// The backspace key.
    Backspace,
    /// The tab key.
    Tab,
    /// The escape key, alone rather than starting a sequence.
    Esc,
    /// An arrow key by direction: true is up, false is down.
    Arrow(bool),
    /// A left arrow key.
    Left,
    /// A right arrow key.
    Right,
    /// The home key.
    Home,
    /// The end key.
    End,
    /// The page-up key.
    PageUp,
    /// The page-down key.
    PageDown,
    /// The delete key.
    Delete,
    /// A shift-tab (back-tab), sent as the application-mode sequence.
    BackTab,
    /// A paste: the text between the bracket markers, delivered as one event
    /// so the run loop never mistakes a newline in a paste for an Enter.
    Paste(String),
    /// A paste bracket opened or closed: the text between is reported as
    /// the Paste event, and the markers themselves are not text.
    PasteMark(bool),
}

/// An incremental decoder over a virtual-terminal byte stream. The pending
/// buffer holds the bytes of an escape sequence a chunk ended halfway
/// through, or the bytes of a paste whose close marker has not arrived.
/// The in-paste flag marks that the pending bytes are paste text rather
/// than an unfinished escape sequence, so the loop treats them right.
#[derive(Default)]
pub struct VtDecoder {
    pending: Vec<u8>,
    in_paste: bool,
}

impl VtDecoder {
    /// Feed one chunk of bytes and take every event it completed. Bytes that
    /// start an escape sequence but do not finish one are held for the next
    /// chunk, as is the tail of a character the chunk ended halfway through.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<VtEvent> {
        // The held bytes come first, so a sequence split across chunks reads
        // as one run. A paste in progress carries its text here too, so the
        // close marker can arrive in the next chunk.
        let mut run = take(&mut self.pending);
        run.extend_from_slice(bytes);
        let paste_in_progress = self.in_paste;
        let mut events = Vec::new();
        let mut text: Vec<u8> = Vec::new();
        // The bytes held for the next chunk: an unfinished escape sequence,
        // or the tail of an unfinished character.
        let mut held: Vec<u8> = Vec::new();
        let mut at = 0;
        while at < run.len() && held.is_empty() {
            let byte = run[at];
            // A paste is in progress: the paste text sits in pending
            // (carried from the previous feed). Collect its bytes until the
            // close marker, holding any tail for the next chunk.
            if paste_in_progress {
                let (paste_text, tail, rest_at) = collect_paste(&run, at);
                if let Some(text) = paste_text {
                    events.push(VtEvent::Paste(text));
                    self.in_paste = false;
                    held = Vec::new();
                    at = rest_at;
                    continue;
                }
                held = tail;
                break;
            }
            match byte {
                0x1b => {
                    // Flush the text collected so the sequence's events keep
                    // their arrival order.
                    held = take_text(&mut text, &mut events);
                    match match_sequence(&run, at) {
                        Some((consumed, Some(event))) => {
                            at += consumed;
                            // A paste bracket opens an accumulator: the
                            // bytes until the close marker are one paste,
                            // so they are not mistaken for keystrokes and
                            // their newlines are not mistaken for Enter.
                            if let VtEvent::PasteMark(true) = event {
                                let (paste_text, _tail, rest_at) = collect_paste(&run, at);
                                if let Some(text) = paste_text {
                                    events.push(VtEvent::Paste(text));
                                    held = Vec::new();
                                    at = rest_at;
                                } else {
                                    // The close marker is in a later
                                    // chunk: hold the paste text and the
                                    // rest of this run for the next feed.
                                    self.in_paste = true;
                                    held = run[at..].to_vec();
                                    at = run.len();
                                }
                            } else {
                                events.push(event);
                            }
                        }
                        Some((consumed, None)) => {
                            // A sequence the decoder consumes but does not
                            // name: its bytes are read and produce nothing.
                            at += consumed;
                        }
                        None => {
                            // An unfinished sequence: hold its bytes ahead of
                            // the held tail and stop reading this run.
                            let mut stop = run[at..].to_vec();
                            stop.extend(take(&mut held));
                            held = stop;
                        }
                    }
                }
                0x0d | 0x0a => {
                    held = take_text(&mut text, &mut events);
                    events.push(VtEvent::Enter);
                    at += 1;
                }
                0x08 | 0x7f => {
                    held = take_text(&mut text, &mut events);
                    events.push(VtEvent::Backspace);
                    at += 1;
                }
                0x09 => {
                    held = take_text(&mut text, &mut events);
                    events.push(VtEvent::Tab);
                    at += 1;
                }
                control @ 0x01..=0x1a => {
                    held = take_text(&mut text, &mut events);
                    // The control block maps to the letter it shares a code
                    // with: 0x03 is ctrl+c, 0x0f is ctrl+o.
                    let letter = (control + b'a' - 1) as char;
                    events.push(VtEvent::Ctrl(letter));
                    at += 1;
                }
                _ => {
                    // A plain byte. The text is collected as raw bytes and
                    // decoded when the run ends, so a multi-byte character
                    // split across chunk boundaries still arrives whole.
                    text.push(byte);
                    at += 1;
                }
            }
        }
        // The tail of a character the run ended halfway through joins the
        // held bytes, if any, for the next chunk.
        let tail = take_text(&mut text, &mut events);
        held.extend(tail);
        self.pending = held;
        events
    }
}

/// Decode the collected plain bytes into character events. Returns the tail
/// bytes of a character the run ended halfway through, for the next chunk
/// to complete.
fn take_text(raw: &mut Vec<u8>, events: &mut Vec<VtEvent>) -> Vec<u8> {
    if raw.is_empty() {
        return Vec::new();
    }
    let valid = match from_utf8(raw) {
        Ok(_) => raw.len(),
        Err(error) => error.valid_up_to(),
    };
    if let Ok(decoded) = from_utf8(&raw[..valid]) {
        for ch in decoded.chars() {
            events.push(VtEvent::Char(ch));
        }
    }
    raw.drain(..valid);
    take(raw)
}

/// Collect the bytes of a paste: everything from the position past the
/// open marker up to the close marker, returned as one paste event's
/// text. Returns None when the close marker has not arrived in this run,
/// in which case the caller holds the tail for the next chunk.
fn collect_paste(run: &[u8], start: usize) -> (Option<String>, Vec<u8>, usize) {
    let close = b"\x1b[201~";
    let body = &run[start..];
    if let Some(pos) = find_subslice(body, close) {
        let text = String::from_utf8_lossy(&body[..pos]).into_owned();
        let held = body[pos + close.len()..].to_vec();
        (Some(text), held, start + pos + close.len())
    } else {
        // The close marker has not arrived yet: hold the whole tail and
        // stop. The caller sets the paste buffer so the next feed continues
        // collecting rather than treating the bytes as keystrokes.
        (None, body.to_vec(), run.len())
    }
}

/// Find the first index of needle in haystack, or None.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Try to read one escape sequence starting at the escape byte. Returns
/// None when the chunk ends before the sequence does, so the caller holds
/// its bytes for the next chunk. Otherwise returns the bytes the sequence
/// consumed and the event it names, which is None for a sequence the
/// decoder consumes but does not name.
fn match_sequence(bytes: &[u8], at: usize) -> Option<(usize, Option<VtEvent>)> {
    let after = at + 1;
    let &next = bytes.get(after)?;
    match next {
        b'[' => {
            // A control sequence: the marker, the parameters, and the final
            // byte. The final byte is a letter for the arrow keys and the
            // tilde for the keypad and paste markers.
            let mut end = after + 1;
            while end < bytes.len() && !bytes[end].is_ascii_alphabetic() && bytes[end] != b'~' {
                end += 1;
            }
            if end >= bytes.len() {
                return None;
            }
            let final_byte = bytes[end];
            let params = &bytes[after + 1..end];
            let consumed = end - at + 1;
            let event = match (final_byte, params) {
                (b'A', _) => Some(VtEvent::Arrow(true)),
                (b'B', _) => Some(VtEvent::Arrow(false)),
                (b'C', _) => Some(VtEvent::Right),
                (b'D', _) => Some(VtEvent::Left),
                (b'H', _) => Some(VtEvent::Home),
                (b'F', _) => Some(VtEvent::End),
                (b'Z', _) => Some(VtEvent::BackTab),
                (b'~', b"1") => Some(VtEvent::Home),
                (b'~', b"4") => Some(VtEvent::End),
                (b'~', b"3") => Some(VtEvent::Delete),
                (b'~', b"5") => Some(VtEvent::PageUp),
                (b'~', b"6") => Some(VtEvent::PageDown),
                (b'~', b"200") => Some(VtEvent::PasteMark(true)),
                (b'~', b"201") => Some(VtEvent::PasteMark(false)),
                _ => None,
            };
            Some((consumed, event))
        }
        b'O' => {
            // A single-letter application key: the arrows and the home/end in
            // the application-mode form.
            let &letter = bytes.get(after + 1)?;
            let event = match letter {
                b'A' => Some(VtEvent::Arrow(true)),
                b'B' => Some(VtEvent::Arrow(false)),
                b'C' => Some(VtEvent::Right),
                b'D' => Some(VtEvent::Left),
                b'H' => Some(VtEvent::Home),
                b'F' => Some(VtEvent::End),
                _ => None,
            };
            Some((3, event))
        }
        // An escape followed by anything else is the escape key alone: the
        // byte behind it is not part of the key, so it stays for the reader.
        _ => Some((1, Some(VtEvent::Esc))),
    }
}

/// Read the bytes a pipe-mode terminal delivers on standard input. The read
/// is blocking: the caller runs it on the reader thread the same way the
/// console path parks in its event source.
pub fn read_pipe_bytes(buffer: &mut [u8]) -> std::io::Result<usize> {
    stdin().read(buffer)
}

#[cfg(test)]
mod tests;
