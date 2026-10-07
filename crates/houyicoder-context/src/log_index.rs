//! The on-disk contract of a session log's offset index.
//!
//! The index is derived data: it holds no fact the log does not, so losing it
//! costs a rescan and never an event. Two consequences shape the rules here.
//! Writing it is best-effort and never fails an append, and the file is not
//! fsynced, because a torn index makes a read slower rather than wrong.
//!
//! The layout is one fixed-size header followed by fixed-size entries. The
//! entry count is derived from the file length instead of being stored, so
//! appending an entry is a single write with no second step updating a count
//! beside it, and an interrupted append cannot leave a count that disagrees
//! with the entries next to it.
//!
//! Sampling is by byte interval and knows nothing about turn boundaries. An
//! entry records where one record starts; a reader looking for a record that
//! falls between two entries walks forward from the earlier one.

use std::fs;
use std::path::Path;

use crate::ids::EventId;

/// Bytes of log between two sampled records. A log of one gibibyte holds four
/// thousand entries at this interval, so the index costs 128 kibibytes for a
/// log eight thousand times its own size.
pub const SAMPLE_INTERVAL_BYTES: u64 = 256 * 1024;

/// Bytes in an index header.
pub const HEADER_LEN: u64 = 44;

/// Bytes in one index entry.
pub const ENTRY_LEN: u64 = 32;

/// The largest index file read into memory. An index holds one entry per
/// sampled interval, so this bound is reached only by a log measured in
/// tebibytes. It exists to cap a read of a file that has been filled with
/// something other than entries.
const MAX_INDEX_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// The first eight bytes of an index file. A file that does not start with
/// them is not an index, whatever else it holds.
const MAGIC: [u8; 8] = *b"HYILOGIX";

/// The layout this module reads and writes. A file recording another version is
/// refused whole rather than partly decoded, because the field widths are what
/// make the entries fixed-size.
const FORMAT_VERSION: u32 = 1;

/// What an absent first event occupies. The header is fixed-width, so absence
/// needs a spelling of the same length, and sixteen zero bytes are the one
/// used. They decode to no first event, which is then compared like any other
/// value: it agrees with a log that held no record when the index was built,
/// and is refused over a log that held one. Absence changes nothing else about
/// the read. The version, the interval and the covered length are checked the
/// same way, and every entry is still readable against the bytes at its own
/// offset.
const ABSENT_EVENT: [u8; 16] = [0; 16];

/// What an index file records about the log it indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogIndexHeader {
    /// The sampling interval the entries were chosen by. A reader uses it to
    /// bound how far it may have to walk from an entry to the record wanted, so a
    /// header recording zero is refused rather than decoded.
    pub interval: u64,
    /// The identity of the log's first record, when the log held one at build
    /// time. The log is append-only, so this never changes: a log starting with
    /// another record is another log, and an index built over the first one
    /// describes bytes that are no longer there.
    pub first_event: Option<EventId>,
    /// How many bytes of the log the index accounted for when it was built. The
    /// log only grows, so a covered length past the log's real length means the
    /// log was shortened or replaced underneath.
    pub covered_len: u64,
}

impl LogIndexHeader {
    /// The header's fixed-width bytes.
    ///
    /// The two counts are little-endian. The event id keeps the big-endian byte
    /// order its own encoding uses, so comparing two ids byte by byte gives the
    /// same answer as comparing them as ids.
    pub fn encode(&self) -> [u8; HEADER_LEN as usize] {
        let mut out = [0u8; HEADER_LEN as usize];
        out[0..8].copy_from_slice(&MAGIC);
        out[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        out[12..20].copy_from_slice(&self.interval.to_le_bytes());
        out[20..36].copy_from_slice(&self.first_event.map_or(ABSENT_EVENT, EventId::to_bytes));
        out[36..44].copy_from_slice(&self.covered_len.to_le_bytes());
        out
    }

    /// Read a header out of a file's leading bytes, which may hold more. None
    /// when the bytes are shorter than a header, start with another magic, record
    /// a version this build cannot read, or record a sampling interval of zero.
    ///
    /// A zero interval is refused rather than carried. The field is the header's
    /// promise about how far apart the entries were sampled, and a reader bounds
    /// its walk between them by it, so a promise of zero says no record can lie
    /// past the entry before it. A writer never makes that promise -- it takes at
    /// least one byte -- so the file holding it has been corrupted, and it is
    /// refused whole rather than read with the field repaired.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let head = bytes.get(..HEADER_LEN as usize)?;
        if head[0..8] != MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(head[8..12].try_into().ok()?);
        if version != FORMAT_VERSION {
            return None;
        }
        let interval = u64::from_le_bytes(head[12..20].try_into().ok()?);
        if interval == 0 {
            return None;
        }
        let raw: [u8; 16] = head[20..36].try_into().ok()?;
        let first_event = (raw != ABSENT_EVENT).then(|| EventId::from_bytes(raw));
        let covered_len = u64::from_le_bytes(head[36..44].try_into().ok()?);
        Some(Self {
            interval,
            first_event,
            covered_len,
        })
    }
}

/// One sampled record's position in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogIndexEntry {
    /// The identity of the record at offset. An offset alone reads as valid
    /// over a log rewritten underneath it, so a reader confirms the bytes there
    /// still hold this record before trusting the number beside it.
    pub event: EventId,
    /// How many records precede this one in the log.
    pub ordinal: u64,
    /// The byte offset this record's line starts at.
    pub offset: u64,
}

impl LogIndexEntry {
    /// The entry's fixed-width bytes.
    pub fn encode(&self) -> [u8; ENTRY_LEN as usize] {
        let mut out = [0u8; ENTRY_LEN as usize];
        out[0..16].copy_from_slice(&self.event.to_bytes());
        out[16..24].copy_from_slice(&self.ordinal.to_le_bytes());
        out[24..32].copy_from_slice(&self.offset.to_le_bytes());
        out
    }

    /// Read one entry out of the leading ENTRY_LEN bytes of a slice, which may
    /// hold more.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let raw: [u8; ENTRY_LEN as usize] = bytes.get(..ENTRY_LEN as usize)?.try_into().ok()?;
        let event = EventId::from_bytes(raw[0..16].try_into().ok()?);
        let ordinal = u64::from_le_bytes(raw[16..24].try_into().ok()?);
        let offset = u64::from_le_bytes(raw[24..32].try_into().ok()?);
        Some(Self {
            event,
            ordinal,
            offset,
        })
    }
}

/// The length an index file of file_len bytes can be read up to, or None when
/// it does not hold a whole header. An append interrupted by a crash leaves
/// part of an entry at the tail; those bytes decode to nothing, so reads bound
/// themselves here and the tail stays invisible instead of being repaired.
pub fn readable_len(file_len: u64) -> Option<u64> {
    let body = file_len.checked_sub(HEADER_LEN)?;
    Some(HEADER_LEN + body - body % ENTRY_LEN)
}

/// How many whole entries an index file of file_len bytes holds. Derived from
/// the length rather than read out of the header, so a file whose entries were
/// appended after a crash still reports the count it can be read to.
pub fn entry_count(file_len: u64) -> Option<u64> {
    readable_len(file_len).map(|len| (len - HEADER_LEN) / ENTRY_LEN)
}

/// Decode the entry n entries into a file's bytes, counting from the first one
/// after the header. None past the whole entries the bytes hold, so a torn tail
/// is never decoded as an entry.
pub fn entry_at(bytes: &[u8], n: u64) -> Option<LogIndexEntry> {
    let whole = readable_len(bytes.len() as u64)?;
    let start = HEADER_LEN.checked_add(n.checked_mul(ENTRY_LEN)?)?;
    // A readable file holds at least a header, which is longer than an entry,
    // so this subtraction cannot underflow.
    if start > whole - ENTRY_LEN {
        return None;
    }
    LogIndexEntry::decode(&bytes[start as usize..])
}

/// An index file read off disk and checked against the log it claims to
/// describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogIndex {
    /// The header the file carried.
    pub header: LogIndexHeader,
    /// The whole entries the file held, in file order. A torn tail entry is not
    /// among them.
    pub entries: Vec<LogIndexEntry>,
}

impl LogIndex {
    /// Read an index file and check it against the log beside it.
    ///
    /// None when the file is missing, unreadable, larger than an index can be,
    /// not an index at all, holding a version this build cannot read or an
    /// interval no writer produces, claiming to cover more of the log than the log
    /// holds, or naming a first record other than the one the log starts with.
    /// Every one of those is answered by rebuilding from the log. Nothing here
    /// repairs a file, because a repair would have to trust the bytes it was
    /// called in to check.
    ///
    /// A covered length below the log's real length is normal rather than a
    /// refusal: the index is built while the log is still growing.
    pub fn load(path: &Path, log_len: u64, log_first_event: Option<EventId>) -> Option<Self> {
        // Checked on the directory entry, so a file filled with something other
        // than entries is refused without first being copied into memory.
        if fs::metadata(path).ok()?.len() > MAX_INDEX_FILE_BYTES {
            return None;
        }
        let bytes = fs::read(path).ok()?;
        let header = LogIndexHeader::decode(&bytes)?;
        if header.covered_len > log_len {
            return None;
        }
        if header.first_event != log_first_event {
            return None;
        }
        let count = entry_count(bytes.len() as u64)?;
        let entries = (0..count)
            .map(|n| entry_at(&bytes, n))
            .collect::<Option<Vec<_>>>()?;
        Some(Self { header, entries })
    }
}

#[cfg(test)]
#[path = "log_index_tests.rs"]
mod log_index_tests;
