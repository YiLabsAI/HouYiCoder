//! Windows runtime checks for the declared legacy code-page path.

use super::*;
use crate::host::text::{OutputEncoding, StreamDecoder, decode_complete};

#[test]
fn test_gbk_decodes_chinese() {
    let bytes = [0xd6, 0xd0, 0xce, 0xc4];
    assert_eq!(decode_code_page(&bytes, 936, true).as_deref(), Some("中文"));
    assert_eq!(
        decode_complete(&bytes, OutputEncoding::LegacyCodePage(936)),
        "中文"
    );
}

#[test]
fn test_gbk_holds_split_pair() {
    let mut decoder = StreamDecoder::new(OutputEncoding::LegacyCodePage(936));
    assert_eq!(decoder.push(&[0xd6]), "");
    assert_eq!(decoder.push(&[0xd0, 0xce, 0xc4]), "中文");
    assert_eq!(decoder.finish(), "");
}
