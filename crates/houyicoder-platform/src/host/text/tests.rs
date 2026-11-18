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
