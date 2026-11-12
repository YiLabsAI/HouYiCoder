//! Tests for the virtual-terminal byte decoder.

use super::VtDecoder;
use super::VtEvent;

/// The control block maps to the letter it shares a code with, so the byte
/// a pipe-mode terminal sends for the fold chord decodes as ctrl+o.
#[test]
fn test_ctrl_o_decodes() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(&[0x0f]);
    assert_eq!(events, vec![VtEvent::Ctrl('o')]);
}

/// The interrupt and end-of-file control bytes decode as their letters too,
/// because the run loop routes them by chord.
#[test]
fn test_ctrl_c_d_decodes() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(&[0x03, 0x04]);
    assert_eq!(events, vec![VtEvent::Ctrl('c'), VtEvent::Ctrl('d')]);
}

/// The arrow keys arrive as escape sequences and decode by direction.
#[test]
fn test_arrows_decode() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(b"\x1b[A\x1b[B");
    assert_eq!(events, vec![VtEvent::Arrow(true), VtEvent::Arrow(false)]);
}

/// The paste bracket delivers its text as one Paste event, so a newline
/// inside a paste is never mistaken for an Enter and the markers themselves
/// never reach the run loop as keystrokes.
#[test]
fn test_paste_bracket_decodes() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(b"\x1b[200~ab\x1b[201~");
    assert_eq!(events, vec![VtEvent::Paste(String::from("ab"))]);
}

/// An escape sequence split across chunks completes when the rest arrives:
/// the first chunk holds its bytes rather than dropping them.
#[test]
fn test_split_sequence_completes() {
    let mut decoder = VtDecoder::default();
    assert!(decoder.feed(b"\x1b[").is_empty());
    let events = decoder.feed(b"A");
    assert_eq!(events, vec![VtEvent::Arrow(true)]);
}

/// A multi-byte character split across chunks arrives as one character, so
/// a paste in a language whose text needs more than one byte per character
/// survives the chunk boundary.
#[test]
fn test_split_character_completes() {
    let mut decoder = VtDecoder::default();
    assert!(decoder.feed(&[0xe4, 0xb8]).is_empty());
    let events = decoder.feed(&[0xad]);
    assert_eq!(events, vec![VtEvent::Char('\u{4e2d}')]);
}

/// Plain text, newlines, and backspace keep their meanings.
#[test]
fn test_text_decodes() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(b"ab\r\x7f");
    assert_eq!(
        events,
        vec![
            VtEvent::Char('a'),
            VtEvent::Char('b'),
            VtEvent::Enter,
            VtEvent::Backspace,
        ]
    );
}

/// A chunk that ends at a bare escape holds the byte rather than deciding:
/// the next chunk tells whether it opens a sequence or stands alone, so a
/// stream that delivers its escape and its sequence in separate reads still
/// decodes one event.
#[test]
fn test_lone_escape_waits() {
    let mut decoder = VtDecoder::default();
    assert!(
        decoder.feed(&[0x1b]).is_empty(),
        "the escape is held until the next byte tells what it opens"
    );
    let events = decoder.feed(b"[A");
    assert_eq!(events, vec![VtEvent::Arrow(true)]);
}

/// A chunk that ends at a bare escape holds the byte, and a byte that is
/// not a sequence opener behind it makes the pair the escape key followed
/// by a character.
#[test]
fn test_escape_then_text() {
    let mut decoder = VtDecoder::default();
    assert!(decoder.feed(&[0x1b]).is_empty());
    let events = decoder.feed(b"a");
    assert_eq!(events, vec![VtEvent::Esc, VtEvent::Char('a')]);
}

/// The application-key form of the arrows decodes by direction, the same
/// as the control-sequence form: a terminal may send either.
#[test]
fn test_application_arrows_decode() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(b"\x1bOA\x1bOB");
    assert_eq!(events, vec![VtEvent::Arrow(true), VtEvent::Arrow(false)]);
}

/// An application key the decoder does not name consumes its bytes and
/// reports no event, so an unknown key does not end the stream.
#[test]
fn test_unknown_application_key_skips() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(b"\x1bOPq");
    assert_eq!(events, vec![VtEvent::Char('q')], "unknown key skipped");
}

/// A paste between the markers arrives as one Paste event whose text keeps
/// the newlines, so the run loop never mistakes a line break for an Enter.
#[test]
fn test_paste_keeps_newlines() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(b"\x1b[200~line one\nline two\x1b[201~");
    assert_eq!(
        events,
        vec![VtEvent::Paste(String::from("line one\nline two"))]
    );
}

/// A tab byte decodes to a Tab event, not to a Ctrl+I, so the run loop
/// routes it to the input it owns.
#[test]
fn test_tab_decodes() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(b"\t");
    assert_eq!(events, vec![VtEvent::Tab]);
}

/// The arrow set in both forms decodes by direction, including left,
/// right, home, end, delete, page-up and page-down.
#[test]
fn test_arrow_set_decodes() {
    let mut decoder = VtDecoder::default();
    let events =
        decoder.feed(b"\x1b[C\x1b[D\x1b[H\x1b[F\x1b[3~\x1b[5~\x1b[6~\x1bOC\x1bOD\x1bOH\x1bOF");
    assert_eq!(
        events,
        vec![
            VtEvent::Right,
            VtEvent::Left,
            VtEvent::Home,
            VtEvent::End,
            VtEvent::Delete,
            VtEvent::PageUp,
            VtEvent::PageDown,
            VtEvent::Right,
            VtEvent::Left,
            VtEvent::Home,
            VtEvent::End,
        ]
    );
}

/// A shift-tab arrives as the application-mode sequence and decodes to a
/// BackTab, so the pane that owns its keys can route it apart from a tab.
#[test]
fn test_backtab_decodes() {
    let mut decoder = VtDecoder::default();
    let events = decoder.feed(b"\x1b[Z");
    assert_eq!(events, vec![VtEvent::BackTab]);
}

/// A paste whose close marker splits across reads still completes: the
/// decoder holds the open marker and the text, and the next read brings
/// the close, so the whole paste arrives as one event.
#[test]
fn test_paste_split_completes() {
    let mut decoder = VtDecoder::default();
    assert!(decoder.feed(b"\x1b[200~text").is_empty());
    let events = decoder.feed(b"\x1b[201~");
    assert_eq!(events, vec![VtEvent::Paste(String::from("text"))]);
}
