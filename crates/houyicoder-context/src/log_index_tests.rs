//! Tests for the on-disk contract of a session log's offset index.

use super::{
    ENTRY_LEN, HEADER_LEN, LogIndex, LogIndexEntry, LogIndexHeader, MAX_INDEX_FILE_BYTES,
    SAMPLE_INTERVAL_BYTES, entry_at, entry_count, readable_len,
};
use crate::ids::EventId;
use std::path::PathBuf;

fn temp_path(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "houyicoder_log_index_{tag}_{}",
        crate::ids::SessionId::new()
    ));
    std::fs::create_dir_all(&dir).expect("create the temp dir");
    dir.join("log.index")
}

fn entry(ordinal: u64) -> LogIndexEntry {
    LogIndexEntry {
        event: EventId::new(),
        ordinal,
        offset: ordinal * 512,
    }
}

fn file_with(header: LogIndexHeader, entries: &[LogIndexEntry]) -> Vec<u8> {
    let mut bytes = header.encode().to_vec();
    for e in entries {
        bytes.extend_from_slice(&e.encode());
    }
    bytes
}

/// A header survives its own encoding, including the sampling interval and
/// the covered length, which are the two numbers a reader bounds itself by.
#[test]
fn test_header_round_trips() {
    let header = LogIndexHeader {
        interval: SAMPLE_INTERVAL_BYTES,
        first_event: Some(EventId::new()),
        covered_len: 9_000_000,
    };
    assert_eq!(LogIndexHeader::decode(&header.encode()), Some(header));
}

/// A log with no records yet has no first event, and an absent one has to
/// survive the same fixed width a present one does.
#[test]
fn test_header_encodes_absence() {
    let header = LogIndexHeader {
        interval: SAMPLE_INTERVAL_BYTES,
        first_event: None,
        covered_len: 0,
    };
    let encoded = header.encode();
    assert_eq!(encoded.len(), HEADER_LEN as usize);
    assert_eq!(encoded[20..36], [0u8; 16]);
    assert_eq!(LogIndexHeader::decode(&encoded), Some(header));
}

/// An entry survives its own encoding, and its width is what makes the
/// entry count derivable from a file length.
#[test]
fn test_entry_round_trips() {
    let e = entry(7);
    assert_eq!(e.encode().len(), ENTRY_LEN as usize);
    assert_eq!(LogIndexEntry::decode(&e.encode()), Some(e));
}

/// Byte order carries identity order, so a reader comparing two encoded ids
/// byte by byte gets the same answer as comparing the ids.
#[test]
fn test_encoded_ids_keep_order() {
    let a = EventId::from_bytes([0u8; 16]);
    let b = EventId::from_bytes([0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(a < b);
    assert!(a.to_bytes() < b.to_bytes());
    assert_eq!(EventId::from_bytes(a.to_bytes()), a);
}

/// A file holding another magic is not an index, and one holding a version
/// this build cannot read is refused whole rather than partly decoded.
#[test]
fn test_decode_refuses_foreign_bytes() {
    let mut bytes = LogIndexHeader {
        interval: SAMPLE_INTERVAL_BYTES,
        first_event: None,
        covered_len: 10,
    }
    .encode()
    .to_vec();
    bytes[0] = b'X';
    assert_eq!(LogIndexHeader::decode(&bytes), None);

    let mut bytes = LogIndexHeader {
        interval: SAMPLE_INTERVAL_BYTES,
        first_event: None,
        covered_len: 10,
    }
    .encode()
    .to_vec();
    bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
    assert_eq!(LogIndexHeader::decode(&bytes), None);

    assert_eq!(LogIndexHeader::decode(&[0u8; 8]), None);
}

/// An interval of zero is refused at both entry points, because the field is the
/// promise a reader bounds its walk by and no writer promises zero bytes. The
/// file is refused rather than read with the field repaired, so a corrupted
/// header costs a rebuild and not a walk that stops at the wrong record.
#[test]
fn test_zero_interval_refused() {
    let mut bytes = LogIndexHeader {
        interval: SAMPLE_INTERVAL_BYTES,
        first_event: None,
        covered_len: 10,
    }
    .encode()
    .to_vec();
    bytes[12..20].copy_from_slice(&0u64.to_le_bytes());
    assert_eq!(LogIndexHeader::decode(&bytes), None);

    let path = temp_path("zero_interval");
    std::fs::write(&path, &bytes).expect("write the index");
    assert_eq!(LogIndex::load(&path, 10, None), None);
    std::fs::remove_file(&path).expect("remove the index");
}

/// The entry count comes out of the file length, and a tail entry only
/// partly written is not counted, so it is never decoded.
#[test]
fn test_torn_tail_invisible() {
    let entries = vec![entry(0), entry(1), entry(2)];
    let bytes = file_with(
        LogIndexHeader {
            interval: SAMPLE_INTERVAL_BYTES,
            first_event: Some(entries[0].event),
            covered_len: 4096,
        },
        &entries,
    );
    let whole = bytes.len() as u64;
    assert_eq!(entry_count(whole), Some(3));

    for torn in 1..ENTRY_LEN {
        let cut = whole - torn;
        assert_eq!(readable_len(cut), Some(whole - ENTRY_LEN));
        assert_eq!(entry_count(cut), Some(2));
        assert_eq!(entry_at(&bytes[..cut as usize], 2), None);
        assert_eq!(entry_at(&bytes[..cut as usize], 1), Some(entries[1]));
    }
    assert_eq!(entry_at(&bytes, 2), Some(entries[2]));
    assert_eq!(entry_at(&bytes, 3), None);
}

/// A file too short to hold a header reports no length and no count, rather
/// than a count of zero that would read as a valid empty index.
#[test]
fn test_short_file_reads_unknown() {
    assert_eq!(readable_len(HEADER_LEN - 1), None);
    assert_eq!(entry_count(HEADER_LEN - 1), None);
    assert_eq!(readable_len(HEADER_LEN), Some(HEADER_LEN));
    assert_eq!(entry_count(HEADER_LEN), Some(0));
    assert_eq!(entry_at(&[0u8; 8], 0), None);
}

/// An index whose log has since been shortened or replaced is refused: a
/// shorter log has no bytes at the offsets the index claims, and a different
/// log has other bytes there, which is the same failure wearing a mask.
#[test]
fn test_stale_index_refused() {
    let path = temp_path("shortened");
    let first = EventId::new();
    let bytes = file_with(
        LogIndexHeader {
            interval: SAMPLE_INTERVAL_BYTES,
            first_event: Some(first),
            covered_len: 4096,
        },
        &[entry(0)],
    );
    std::fs::write(&path, &bytes).expect("write the index");

    assert!(LogIndex::load(&path, 4096, Some(first)).is_some());
    assert_eq!(LogIndex::load(&path, 4095, Some(first)), None);
    assert_eq!(LogIndex::load(&path, 4096, Some(EventId::new())), None);
    assert_eq!(LogIndex::load(&path, 4096, None), None);
    std::fs::remove_file(&path).expect("remove the index");
}

/// The size bound alone refuses this file: the same header loads at the
/// length it was written at, and is refused once the file claims more bytes
/// than a log could produce an index for. The claim is made without writing
/// them, which is the point -- a reader that copied the file in first would
/// have paid for all of them before deciding.
#[test]
fn test_oversized_file_refused() {
    let path = temp_path("oversized");
    let first = EventId::new();
    let bytes = file_with(
        LogIndexHeader {
            interval: SAMPLE_INTERVAL_BYTES,
            first_event: Some(first),
            covered_len: 512,
        },
        &[entry(0)],
    );
    std::fs::write(&path, &bytes).expect("write the index");
    assert!(LogIndex::load(&path, 512, Some(first)).is_some());

    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("reopen the index");
    file.set_len(MAX_INDEX_FILE_BYTES + 1)
        .expect("claim a size no log produces");
    drop(file);
    assert_eq!(LogIndex::load(&path, 512, Some(first)), None);
    std::fs::remove_file(&path).expect("remove the index");
}

/// A log that has grown since the index was built is the normal case, and
/// the index is still read: it covers a prefix, and a reader walks the rest.
#[test]
fn test_grown_log_still_loads() {
    let path = temp_path("grown");
    let first = EventId::new();
    let entries = vec![entry(0), entry(40)];
    let bytes = file_with(
        LogIndexHeader {
            interval: SAMPLE_INTERVAL_BYTES,
            first_event: Some(first),
            covered_len: 20_480,
        },
        &entries,
    );
    std::fs::write(&path, &bytes).expect("write the index");

    let loaded = LogIndex::load(&path, 999_999, Some(first)).expect("load the index");
    assert_eq!(loaded.entries, entries);
    assert_eq!(loaded.header.covered_len, 20_480);
    std::fs::remove_file(&path).expect("remove the index");
}

/// A torn tail survives a load: the whole entries are returned and the part
/// of one is dropped, so a crash mid-append costs the newest sample and
/// nothing else.
#[test]
fn test_load_drops_torn_tail() {
    let path = temp_path("torn");
    let first = EventId::new();
    let entries = vec![entry(0), entry(30), entry(60)];
    let mut bytes = file_with(
        LogIndexHeader {
            interval: SAMPLE_INTERVAL_BYTES,
            first_event: Some(first),
            covered_len: 40_000,
        },
        &entries,
    );
    bytes.truncate(bytes.len() - 5);
    std::fs::write(&path, &bytes).expect("write the index");

    let loaded = LogIndex::load(&path, 40_000, Some(first)).expect("load the index");
    assert_eq!(loaded.entries, entries[..2].to_vec());
    std::fs::remove_file(&path).expect("remove the index");
}

/// A missing index is not an error and not an empty one: the caller has no
/// file to trust and rebuilds from the log.
#[test]
fn test_missing_file_refused() {
    let path = temp_path("missing");
    assert_eq!(LogIndex::load(&path, 100, None), None);
    std::fs::remove_dir_all(path.parent().expect("a parent")).ok();
}

/// An index over an empty log holds no first event and no entries, which is
/// a valid file rather than a missing one.
#[test]
fn test_empty_log_index_loads() {
    let path = temp_path("empty");
    let bytes = file_with(
        LogIndexHeader {
            interval: SAMPLE_INTERVAL_BYTES,
            first_event: None,
            covered_len: 0,
        },
        &[],
    );
    std::fs::write(&path, &bytes).expect("write the index");
    let loaded = LogIndex::load(&path, 0, None).expect("load the index");
    assert!(loaded.entries.is_empty());
    std::fs::remove_file(&path).expect("remove the index");
}
