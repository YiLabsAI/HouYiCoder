//! Decoder tests over chunk boundaries and declared encodings.

use super::*;

#[test]
fn test_utf8_holds_split_codepoint() {
    let mut decoder = StreamDecoder::new(OutputEncoding::Utf8);
    let snowman = "☃".as_bytes();
    assert_eq!(decoder.push(&snowman[..2]), "");
    assert_eq!(decoder.push(&snowman[2..]), "☃");
    assert_eq!(decoder.finish(), "");
}

#[test]
fn test_utf8_replaces_bad_byte() {
    let mut decoder = StreamDecoder::new(OutputEncoding::Utf8);
    assert_eq!(decoder.push(b"ok \xff done\n"), "ok � done\n");
    assert_eq!(decoder.finish(), "");
}

#[test]
fn test_utf16_holds_odd_byte() {
    let mut decoder = StreamDecoder::new(OutputEncoding::Utf16Le);
    let bytes: Vec<u8> = "中文".encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert_eq!(decoder.push(&bytes[..3]), "中");
    assert_eq!(decoder.push(&bytes[3..]), "文");
    assert_eq!(decoder.finish(), "");
}

#[test]
fn test_utf16_holds_high_surrogate() {
    let mut decoder = StreamDecoder::new(OutputEncoding::Utf16Le);
    let bytes: Vec<u8> = "😀".encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert_eq!(decoder.push(&bytes[..2]), "");
    assert_eq!(decoder.push(&bytes[2..]), "😀");
    assert_eq!(decoder.finish(), "");
}

#[test]
fn test_utf16_skips_split_bom() {
    let mut decoder = StreamDecoder::new(OutputEncoding::Utf16Le);
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend("中".encode_utf16().flat_map(u16::to_le_bytes));
    assert_eq!(decoder.push(&bytes[..1]), "");
    assert_eq!(decoder.push(&bytes[1..]), "中");
}

#[test]
fn test_utf16_finish_odd_byte() {
    let mut decoder = StreamDecoder::new(OutputEncoding::Utf16Le);
    assert_eq!(decoder.push(&[0x2d]), "");
    assert_eq!(decoder.finish(), "�");
}

/// A byte pair that spells a byte order mark after the stream has started is
/// a character the producer wrote, not a marker to drop. Only the leading one
/// is removed.
#[test]
fn test_utf16_keeps_later_bom() {
    let mut decoder = StreamDecoder::new(OutputEncoding::Utf16Le);
    let mut bytes: Vec<u8> = "a".encode_utf16().flat_map(u16::to_le_bytes).collect();
    bytes.extend([0xff, 0xfe]);
    bytes.extend("b".encode_utf16().flat_map(u16::to_le_bytes));
    assert_eq!(decoder.push(&bytes), "a\u{feff}b");
    assert_eq!(decoder.finish(), "");
}

/// A stream that begins with the marker has it removed once, and the same
/// bytes later in the stream stay.
#[test]
fn test_utf16_strips_leading_bom() {
    let mut decoder = StreamDecoder::new(OutputEncoding::Utf16Le);
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend("中".encode_utf16().flat_map(u16::to_le_bytes));
    bytes.extend([0xff, 0xfe]);
    assert_eq!(decoder.push(&bytes), "中\u{feff}");
    assert_eq!(decoder.finish(), "");
}

/// The report names the encoding it read under and whether any byte had to
/// be replaced, so a caller can persist both facts next to the text.
#[test]
fn test_decode_report_names_encoding() {
    let clean = decode_complete_report(b"ready\n", OutputEncoding::Utf8);
    assert_eq!(clean.text, "ready\n");
    assert_eq!(clean.encoding, OutputEncoding::Utf8);
    assert!(!clean.had_errors);

    let broken = decode_complete_report(b"ok \xff\n", OutputEncoding::Utf8);
    assert_eq!(broken.text, "ok \u{fffd}\n");
    assert!(broken.had_errors);

    let utf16: Vec<u8> = "中文".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let wide = decode_complete_report(&utf16, OutputEncoding::Utf16Le);
    assert_eq!(wide.text, "中文");
    assert_eq!(wide.encoding, OutputEncoding::Utf16Le);
    assert!(!wide.had_errors);

    let odd = decode_complete_report(&[0x2d], OutputEncoding::Utf16Le);
    assert_eq!(odd.text, "\u{fffd}");
    assert!(odd.had_errors);
}

/// A window reader steps one character at a time, so each decoded character
/// reports the stored bytes it came from.
#[test]
fn test_next_char_steps_units() {
    let bytes: Vec<u8> = "😀a".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let (ch, used) = next_char(&bytes, OutputEncoding::Utf16Le).expect("a whole pair");
    assert_eq!(ch, "😀");
    assert_eq!(used, 4);
    let (ch, used) = next_char(&bytes[4..], OutputEncoding::Utf16Le).expect("a whole unit");
    assert_eq!(ch, "a");
    assert_eq!(used, 2);
    assert_eq!(
        next_char(&bytes[..3], OutputEncoding::Utf16Le),
        None,
        "a pair the buffer cut is held for the next window"
    );

    let (ch, used) = next_char("中x".as_bytes(), OutputEncoding::Utf8).expect("a whole character");
    assert_eq!((ch.as_str(), used), ("中", 3));
    assert_eq!(
        next_char(&"中".as_bytes()[..2], OutputEncoding::Utf8),
        None,
        "a character the buffer cut is held for the next window"
    );
}

/// A double-byte stream cut at a mid-character offset resumes after the last
/// single-byte character, not a fixed number of bytes back: any byte below
/// 0x40 is such a character and cannot be a trail byte, so it anchors the
/// alignment. Without one the window start is the only safe resume point.
#[test]
fn test_resync_finds_anchor() {
    // A newline, then the bytes of repeated double-byte characters.
    let mut bytes = vec![b'\n'];
    bytes.extend([0xd6, 0xd0, 0xd6, 0xd0, 0xd6, 0xd0]);
    assert_eq!(
        resync_offset(&bytes, 4),
        1,
        "resume right after the newline"
    );
    assert_eq!(resync_offset(&bytes, 7), 1, "the anchor is unchanged later");
    // No single-byte character in the window: the block start is the answer.
    assert_eq!(resync_offset(&[0xd6, 0xd0, 0xd6], 3), 0);
    // A limit past the buffer clamps instead of panicking.
    assert_eq!(resync_offset(&bytes, 99), 1);
}

#[test]
fn test_legacy_holds_lead_bytes() {
    let gbk = [0xd6, 0xd0, 0xce, 0xc4];
    let strict = |bytes: &[u8]| match bytes {
        [] => Some(String::new()),
        [0xd6, 0xd0, 0xce, 0xc4] => Some("中文".to_string()),
        _ => None,
    };
    let (text, consumed) = legacy_prefix(&gbk[..2], strict, |_| "bad".to_string());
    assert_eq!(text, "");
    assert_eq!(consumed, 0);
    let (text, consumed) = legacy_prefix(&gbk, strict, |_| "bad".to_string());
    assert_eq!(text, "中文");
    assert_eq!(consumed, gbk.len());
}

#[test]
fn test_legacy_replaces_bad_sequence() {
    let bytes = [0xff, 0x61];
    let (text, consumed) = legacy_prefix(&bytes, |_| None, |_| "�a".to_string());
    assert_eq!(text, "�a");
    assert_eq!(consumed, bytes.len());
}

#[test]
fn test_shells_negotiate_utf8() {
    assert_eq!(
        output_encoding_for_shell(Shell::Posix),
        OutputEncoding::Utf8
    );
    assert_eq!(
        output_encoding_for_shell(Shell::GitBash),
        OutputEncoding::Utf8
    );
    assert_eq!(
        output_encoding_for_shell(Shell::PowerShell(crate::host::shell::PwshEdition::Core)),
        OutputEncoding::Utf8
    );
}

#[cfg(not(target_os = "windows"))]
#[test]
fn test_cmd_uses_utf8() {
    assert_eq!(output_encoding_for_shell(Shell::Cmd), OutputEncoding::Utf8);
}
