//! The fold: what the log says each stored result is.
//!
//! A tool result names its kept bytes in one of two ways. A capture publishes a
//! record block that says which call produced which stream, and a marker left by
//! the output isolation step names the block that replaced a large field. The
//! fold reads both, in log order, so the index it builds is ordered the way the
//! work happened.

use std::collections::HashMap;

use houyicoder_api::result_catalog::ResultCatalogError;
use houyicoder_api::session::{SessionLog, TrajectoryPosition};
use houyicoder_context::{BlockHash, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;
use houyicoder_protocol::capture_record::{
    CAPTURE_KEY, CaptureCompleteness, CaptureRecord, RECORD_KEY, STDERR_KEY, STDOUT_KEY,
    StreamEncoding, StreamRecord, read_capture_record,
};
use houyicoder_protocol::result_catalog::{
    ResultCompleteness, ResultEncoding, ResultEntry, ResultId, ResultStream,
};
use serde_json::Value;

use super::entry::{BlockSource, IndexedResult, StoredShape};
use super::windows;

/// Records one page of the log carries. Each page is released as the next is
/// read, so folding a long session costs one page however long it is.
const PAGE_RECORDS: usize = 512;

/// The most bytes a capture record may occupy. The record names streams and
/// their sizes; it never carries the bytes themselves, so a larger block is
/// refused rather than read into memory.
const RECORD_BYTES: usize = 64 * 1024;

/// How many characters of a command a listing row shows. A row is read beside
/// other rows, so a long command line is cut rather than wrapped over them.
const COMMAND_CHARS: usize = 160;

/// What one session's fold keeps between requests.
#[derive(Debug, Default)]
pub(super) struct Index {
    /// The clear boundary the log state began at when this index was folded.
    ///
    /// A clear replaces the log state and starts a new epoch, so an index
    /// folded before it describes records the session no longer holds. The
    /// boundary is what says which epoch the index is a view of.
    pub epoch: Option<String>,
    /// Where the next page read continues.
    pub position: TrajectoryPosition,
    /// The results found so far, in log order.
    pub results: Vec<IndexedResult>,
    /// Where each result sits in the list, by the name it answers to.
    by_name: HashMap<String, usize>,
    /// What the log said about every call folded so far. A result event
    /// carries neither the tool nor the command, so the call it answers is
    /// what says which one ran and with what input.
    calls: HashMap<String, CallDetails>,
    /// Results the fold could not read.
    pub corrupt: usize,
    /// Bytes a page read could not parse.
    pub skipped_bytes: u64,
    /// Whether the fold reached the end of the log on the last refresh.
    pub complete: bool,
    /// The id of the newest record folded, which is what the index is a view
    /// of.
    pub as_of: Option<String>,
}

/// What the log said about one call.
#[derive(Debug, Clone)]
struct CallDetails {
    /// The tool the call ran.
    tool: String,
    /// The command line a shell call ran, absent for every other tool.
    command: Option<String>,
}

/// What one page contributes to the index, held aside until the whole page
/// has been folded.
///
/// A request can be dropped while it waits on a block read. Applying a page
/// only after every record of it has been read means a dropped request leaves
/// the index exactly as it was, so the page is folded again rather than
/// half-absorbed and passed over.
#[derive(Default)]
struct Folded {
    /// Calls the page named, in page order.
    calls: Vec<(String, CallDetails)>,
    /// Results the page stored.
    results: Vec<IndexedResult>,
    /// Records the page could not read.
    corrupt: usize,
}

impl Folded {
    /// What the log said about one call. The page's own calls are read first,
    /// so a result folded after its call finds it within the same page.
    fn call<'a>(&'a self, index: &'a Index, call_id: &str) -> Option<&'a CallDetails> {
        self.calls
            .iter()
            .rev()
            .find(|(id, _)| id == call_id)
            .map(|(_, info)| info)
            .or_else(|| index.calls.get(call_id))
    }
}

impl Index {
    /// The list position of a result, by the name it answers to.
    pub(super) fn lookup(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }

    /// Whether the index is a complete record of the session's log: every
    /// record of it was read and every one of them folded. An answer that
    /// reports coverage from this is not contradicted by a record the fold
    /// could not read.
    pub(super) fn covers_whole_log(&self) -> bool {
        self.complete && self.corrupt == 0 && self.skipped_bytes == 0
    }

    /// Take what one page folded. Everything the page contributes lands here,
    /// so the index either holds the page or none of it.
    fn apply(&mut self, folded: Folded) {
        for (call, info) in folded.calls {
            self.calls.insert(call, info);
        }
        for result in folded.results {
            self.push(result);
        }
        self.corrupt += folded.corrupt;
    }

    /// Start the index over for the epoch the log state now begins at.
    fn reset(&mut self, epoch: Option<String>) {
        self.position = TrajectoryPosition::start();
        self.results.clear();
        self.by_name.clear();
        self.calls.clear();
        self.corrupt = 0;
        self.skipped_bytes = 0;
        self.complete = false;
        self.as_of = None;
        self.epoch = epoch;
    }

    fn push(&mut self, result: IndexedResult) {
        let name = result.entry.id.as_str().to_string();
        if let Some(earlier) = self.by_name.insert(name.clone(), self.results.len()) {
            // Two results answering to one name would hide the earlier one
            // from every read, so the collision is reported rather than
            // absorbed.
            tracing::warn!(id = name, earlier, "two results answer to one name");
        }
        self.results.push(result);
    }
}

/// Bring the index up to the end of the session's log.
///
/// Every refresh reads at least one page, so a result appended since the last
/// call is found without the caller tracking log growth itself.
pub(super) async fn refresh(
    log: &dyn SessionLog,
    session: SessionId,
    index: &mut Index,
) -> Result<(), ResultCatalogError> {
    // A clear drops the log state and begins a new epoch, whose records start
    // over at the boundary. Folding on from a position that belongs to the
    // epoch before it would read an empty page and take that for the end of a
    // log the index no longer describes.
    let epoch = log
        .trajectory_head(session)
        .revision
        .epoch_event_id
        .map(|id| id.to_string());
    if index.epoch != epoch {
        index.reset(epoch);
    }
    loop {
        let page = log
            .trajectory_page(session, index.position, PAGE_RECORDS)
            .map_err(|e| ResultCatalogError::Store(e.to_string()))?;
        let moved = page.next != index.position;
        if page.events.is_empty() {
            // No record is read before the position moves, so the page lands
            // whole: a request dropped here leaves the index as it was.
            index.skipped_bytes += page.skipped_bytes;
            if !moved {
                index.complete = true;
                return Ok(());
            }
            // A page can hold no record and still advance: the first record
            // of it was passed over as wider than one read. The fold goes on
            // from the new position rather than taking the gap for the end of
            // the log.
            index.position = page.next;
            index.complete = false;
            continue;
        }
        let mut folded = Folded::default();
        for record in &page.events {
            absorb(log, record, index, &mut folded).await;
        }
        index.apply(folded);
        index.skipped_bytes += page.skipped_bytes;
        index.complete = page.events.len() < PAGE_RECORDS;
        index.position = page.next;
        if let Some(last) = page.events.last() {
            index.as_of = Some(last.id.to_string());
        }
        if !moved {
            // The store cannot serve past this moment, so the fold reports
            // what it reached rather than asking again for the same page.
            index.complete = false;
            return Ok(());
        }
    }
}

/// Fold one log record into the page's staging area.
async fn absorb(
    log: &dyn SessionLog,
    record: &SessionLogEntry,
    index: &Index,
    folded: &mut Folded,
) {
    match &record.event {
        SessionEvent::ToolCall {
            call_id,
            tool,
            input,
        } => {
            folded.calls.push((
                call_id.clone(),
                CallDetails {
                    tool: tool.clone(),
                    command: command_of(input),
                },
            ));
        }
        SessionEvent::ToolResult {
            call_id, output, ..
        } => {
            if absorb_capture(log, record, call_id, output, index, folded).await {
                // The capture kept the raw bytes, and the bounded copy of the
                // same output is what the transcript already shows. Listing it
                // again would offer the model the copy it can already read.
                return;
            }
            absorb_markers(log, record, call_id, output, index, folded).await;
        }
        _ => {}
    }
}

/// The command a call ran, as a listing row can show it: one line, bounded in
/// length. Absent for a tool that has no command.
fn command_of(input: &Value) -> Option<String> {
    let command = input.get("command")?.as_str()?.trim();
    let first = command.lines().next()?;
    if first.is_empty() {
        return None;
    }
    let mut text: String = first.chars().take(COMMAND_CHARS).collect();
    if first.chars().count() > COMMAND_CHARS {
        text.push_str("...");
    }
    Some(text)
}

/// Fold a result that names a capture record. True when the record was read,
/// whether or not it named a stream with bytes.
async fn absorb_capture(
    log: &dyn SessionLog,
    record: &SessionLogEntry,
    call_id: &str,
    output: &Value,
    index: &Index,
    folded: &mut Folded,
) -> bool {
    let Some(capture) = output.get(CAPTURE_KEY) else {
        return false;
    };
    let Some(block) = capture
        .get(RECORD_KEY)
        .and_then(|marker| marker.get(BLOCK_REF_KEY))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let bytes = match record_bytes(log, &BlockHash(block.to_string())).await {
        Ok(bytes) => bytes,
        Err(error) => {
            folded.corrupt += 1;
            tracing::warn!(block, %error, "capture record did not read");
            return false;
        }
    };
    let capture = match read_capture_record(&bytes) {
        Ok(capture) => capture,
        Err(error) => {
            folded.corrupt += 1;
            tracing::warn!(block, %error, "capture record did not parse");
            return false;
        }
    };
    if capture.session != record.session.to_string() || capture.call != call_id {
        // A record that names another session or another call did not come
        // from this log's capture of this call, and listing it would answer
        // this scope with bytes it does not own.
        folded.corrupt += 1;
        tracing::warn!(
            call = capture.call,
            session = capture.session,
            "capture record names another call"
        );
        return false;
    }
    let info = folded.call(index, &capture.call).cloned();
    push_stream(
        record,
        &capture,
        ResultStream::Stdout,
        &capture.stdout,
        info.as_ref(),
        folded,
    );
    push_stream(
        record,
        &capture,
        ResultStream::Stderr,
        &capture.stderr,
        info.as_ref(),
        folded,
    );
    true
}

fn push_stream(
    record: &SessionLogEntry,
    capture: &CaptureRecord,
    stream: ResultStream,
    side: &StreamRecord,
    info: Option<&CallDetails>,
    folded: &mut Folded,
) {
    let Some(block) = side.block.as_ref() else {
        return;
    };
    let entry = ResultEntry {
        id: stream_id(&capture.call, &stream),
        tool: Some(capture.tool.clone()),
        command: info.and_then(|info| info.command.clone()),
        event: record.id.to_string(),
        call: Some(capture.call.clone()).filter(|call| !call.is_empty()),
        created_ms: capture.created_ms,
        exit: capture.exit,
        completeness: completeness(capture.completeness),
        stop_reason: capture.stop_reason,
        stream,
        encoding: encoding_of(side.encoding),
        bytes: Some(side.bytes),
    };
    folded.results.push(IndexedResult::new(
        entry,
        BlockSource {
            hash: BlockHash(block.hash.clone()),
            shape: StoredShape::Raw,
        },
    ));
}

/// The name one captured stream answers to.
fn stream_id(call: &str, stream: &ResultStream) -> ResultId {
    let name = match stream {
        ResultStream::Stdout => STDOUT_KEY,
        ResultStream::Stderr => STDERR_KEY,
        _ => "stream",
    };
    ResultId::new(format!("{call}:{name}"))
}

/// The reading a capture's stored encoding asks for.
fn encoding_of(encoding: StreamEncoding) -> ResultEncoding {
    match encoding {
        StreamEncoding::Raw => ResultEncoding::Raw,
        // An encoding this reader does not know is not known to be plain
        // bytes, so it reads as a stored form rather than being shown as
        // though the bytes were the text.
        _ => ResultEncoding::JsonStored,
    }
}

/// Fold a result whose output carries markers from the output isolation step.
async fn absorb_markers(
    log: &dyn SessionLog,
    record: &SessionLogEntry,
    call_id: &str,
    output: &Value,
    index: &Index,
    folded: &mut Folded,
) {
    let Some(object) = output.as_object() else {
        return;
    };
    // A whole-output marker replaces the object that held the fields, so it
    // sits at the top level and there is no field to name.
    if let Some(block) = object.get(BLOCK_REF_KEY).and_then(Value::as_str) {
        push_stored(log, record, call_id, "", block, index, folded).await;
        return;
    }
    for (field, value) in object {
        if field == CAPTURE_KEY {
            continue;
        }
        let Some(block) = value.get(BLOCK_REF_KEY).and_then(Value::as_str) else {
            continue;
        };
        push_stored(log, record, call_id, field, block, index, folded).await;
    }
}

async fn push_stored(
    log: &dyn SessionLog,
    record: &SessionLogEntry,
    call_id: &str,
    field: &str,
    block: &str,
    index: &Index,
    folded: &mut Folded,
) {
    let hash = BlockHash(block.to_string());
    let (shape, total) = match windows::peek(log.backend(), &hash).await {
        Ok(found) => found,
        Err(error) => {
            folded.corrupt += 1;
            tracing::warn!(block, %error, "stored result block did not read");
            return;
        }
    };
    let name = if field.is_empty() {
        format!("{}:output", record.id)
    } else {
        format!("{}:{field}", record.id)
    };
    let info = folded.call(index, call_id);
    let entry = ResultEntry {
        id: ResultId::new(name),
        tool: info.map(|info| info.tool.clone()),
        command: info.and_then(|info| info.command.clone()),
        event: record.id.to_string(),
        call: Some(call_id.to_string()).filter(|call| !call.is_empty()),
        created_ms: Some(record.ts),
        exit: None,
        completeness: ResultCompleteness::Unknown,
        stop_reason: None,
        stream: ResultStream::Stored {
            field: field.to_string(),
        },
        encoding: ResultEncoding::JsonStored,
        // The stored form of a string field is the escaped JSON, so its length
        // is not the length of the text the producer wrote and is left absent
        // rather than reported as a wrong count.
        bytes: match shape {
            StoredShape::JsonString => None,
            StoredShape::Raw | StoredShape::JsonText => Some(total),
        },
    };
    folded
        .results
        .push(IndexedResult::new(entry, BlockSource { hash, shape }));
}

/// Read a capture record block whole.
async fn record_bytes(
    log: &dyn SessionLog,
    hash: &BlockHash,
) -> Result<Vec<u8>, ResultCatalogError> {
    let window = log
        .backend()
        .block_get_range(hash, 0, RECORD_BYTES)
        .await
        .map_err(|error| windows::read_error(hash, error))?;
    if window.total > window.bytes.len() as u64 {
        return Err(ResultCatalogError::Corrupt(
            "the capture record is larger than a record may be".into(),
        ));
    }
    Ok(window.bytes)
}

fn completeness(capture: CaptureCompleteness) -> ResultCompleteness {
    match capture {
        CaptureCompleteness::Complete => ResultCompleteness::Complete,
        CaptureCompleteness::Truncated => ResultCompleteness::Truncated,
        // A completeness value this build does not know says only that the
        // kept bytes are not known to be whole.
        _ => ResultCompleteness::Unknown,
    }
}
