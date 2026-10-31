//! The per-session index behind the result catalog port.
//!
//! One index per session is folded from that session's log on first use and
//! extended on every later request, so a request costs the records appended
//! since the last one rather than a replay. The index is held behind an async
//! lock because folding reads blocks from the store; a read or a search copies
//! what it needs out of the index and releases the lock before it touches the
//! store again.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, PoisonError};

use tokio::sync::Mutex;

use houyicoder_api::result_catalog::{ResultCatalog, ResultCatalogError};
use houyicoder_api::session::SessionLog;
use houyicoder_async::PFut;
use houyicoder_context::{ContextBackend, SessionId};
use houyicoder_protocol::result_catalog::{
    ByteSpan, MAX_PAGE_ENTRIES, MAX_SEARCH_HITS, ResultCompleteness, ResultContent, ResultEntry,
    ResultFilter, ResultListQuery, ResultPage, ResultRange, ResultRead, ResultReadQuery,
    ResultSearchPage, ResultSearchQuery, ResultStats, ResultStream, ScanBudget,
};

use super::cursor::{self, Operation};
use super::entry::IndexedResult;
use super::fold::{self, Index};
use super::search::{self, DEFAULT_SCAN_BYTES, DEFAULT_SCAN_MILLIS, Target};
use super::windows::{self, DecodedWindow, WINDOW_BYTES};

/// How many results a listing page carries when the caller asks for no
/// particular size.
const DEFAULT_PAGE: usize = MAX_PAGE_ENTRIES as usize;

/// The most lines one line read returns, so one call cannot pull a whole
/// stream into the model window.
const MAX_LINES: u64 = 200;

/// The most bytes one line read returns.
///
/// A line can be longer than a page: a minified file or a log written without
/// newlines is one line of any length. The bound is the length of one whole
/// stream a reader is shown in one answer, so a long line is paged like any
/// other text and the continuation names the line it stopped in.
const MAX_LINE_BYTES: usize = 8000;

/// The bytes one line read scans to find its starting line. A read that would
/// scan further refuses rather than paging through a very large result for a
/// line far past anything a reader asked to see.
const LINE_SCAN_BYTES: u64 = 8 * 1024 * 1024;

/// The most bytes one character needs. A page that stops at a character
/// boundary can end up to this many bytes past its even budget, so the budget
/// reserves them rather than cutting the answer short.
const LINE_SLACK: usize = 4;

/// What every cursor one read issues is bound to: the session it answers for,
/// the log epoch its index is a view of, and the request the position
/// continues.
#[derive(Debug, Clone, Copy)]
struct CursorBinding<'a> {
    scope: SessionId,
    epoch: &'a str,
    request: u64,
}

/// The host's view of every session's stored results.
pub struct SessionResultCatalog {
    log: Arc<dyn SessionLog>,
    indexes: Mutex<HashMap<SessionId, Index>>,
}

impl SessionResultCatalog {
    /// Build a catalog that folds the given log's sessions.
    pub fn new(log: Arc<dyn SessionLog>) -> Self {
        Self {
            log,
            indexes: Mutex::new(HashMap::new()),
        }
    }

    fn backend(&self) -> &dyn ContextBackend {
        self.log.backend()
    }

    async fn serve_read(
        &self,
        binding: CursorBinding<'_>,
        item: IndexedResult,
        query: ResultReadQuery,
    ) -> Result<ResultRead, ResultCatalogError> {
        let id = query.id.as_str().to_string();
        match &query.range {
            ResultRange::Bytes { offset, len } => {
                let request = cursor::request(&[&id, "bytes"]);
                let binding = CursorBinding { request, ..binding };
                let start = match &query.cursor {
                    Some(at) => {
                        cursor::parse(
                            at,
                            binding.scope,
                            binding.epoch,
                            Operation::Read,
                            request,
                            usize::MAX,
                        )?
                        .offset
                    }
                    None => *offset,
                };
                let window =
                    windows::read_window(self.backend(), &item.source, start, *len as usize)
                        .await?;
                if start > window.total {
                    // A read that begins past the end is a position the caller
                    // asked for by mistake, not a stream with nothing left.
                    return Err(ResultCatalogError::InvalidRange(format!(
                        "byte {start} is past the end of this result, which holds {} bytes",
                        window.total
                    )));
                }
                let window = window.take_bytes(u64::from(*len));
                Ok(assemble_read(binding, &item, window, None))
            }
            ResultRange::Lines { start, count } => {
                let request = cursor::request(&[&id, "lines"]);
                let binding = CursorBinding { request, ..binding };
                let resume = match &query.cursor {
                    Some(at) => {
                        let decoded = cursor::parse(
                            at,
                            binding.scope,
                            binding.epoch,
                            Operation::Read,
                            request,
                            usize::MAX,
                        )?;
                        Some((decoded.entry as u64, decoded.offset))
                    }
                    None => None,
                };
                self.serve_lines(binding, &item, *start, u64::from(*count), resume)
                    .await
            }
            _ => Err(ResultCatalogError::InvalidRange(
                "this build does not know the range unit".into(),
            )),
        }
    }

    /// Read a range of lines, using the marks a previous read left behind so
    /// a second read starts near its line rather than at the block.
    ///
    /// Lines count from one. A cursor names the line and the stored offset a
    /// previous page stopped at and takes precedence over the range's start,
    /// so a caller continues where the pages actually are rather than
    /// recomputing the position itself.
    async fn serve_lines(
        &self,
        binding: CursorBinding<'_>,
        item: &IndexedResult,
        start: u64,
        count: u64,
        resume: Option<(u64, u64)>,
    ) -> Result<ResultRead, ResultCatalogError> {
        let count = count.clamp(1, MAX_LINES);
        let (line, offset) = match resume {
            Some((line, offset)) => (line, offset),
            None => {
                if start == 0 {
                    return Err(ResultCatalogError::InvalidRange(
                        "lines count from one, and line 1 is the first line".into(),
                    ));
                }
                self.locate_line(item, start).await?
            }
        };
        if line < start {
            return Err(ResultCatalogError::InvalidRange(format!(
                "line {start} is past the end of this result"
            )));
        }
        let first = line;
        let begin = offset;
        let mut text = String::new();
        let mut lines = 0u64;
        let mut scanned = 0u64;
        let mut cursor = offset;
        let mut total;
        loop {
            let room = MAX_LINE_BYTES
                .saturating_sub(LINE_SLACK)
                .saturating_sub(text.len());
            let want = room.clamp(1, WINDOW_BYTES);
            let window = windows::read_window(self.backend(), &item.source, cursor, want).await?;
            total = window.total;
            if window.end <= cursor {
                break;
            }
            scanned += window.end - window.start;
            // The window stops at the line the page ends on or at the byte a
            // page holds, whichever comes first, and always at a character
            // boundary so a continuation never starts inside one.
            let mut cut = window.text.len().min(room);
            while cut < window.text.len() && !window.text.is_char_boundary(cut) {
                cut += 1;
            }
            for (at, ch) in window.text.char_indices() {
                if at >= cut {
                    break;
                }
                if ch == '\n' {
                    lines += 1;
                    if lines == count {
                        cut = at + 1;
                        break;
                    }
                }
            }
            text.push_str(&window.text[..cut]);
            cursor = if cut < window.text.len() {
                window.stored_offset(cut)
            } else {
                window.end
            };
            if lines == count
                || text.len() + LINE_SLACK >= MAX_LINE_BYTES
                || scanned >= LINE_SCAN_BYTES
                || cursor >= window.total
            {
                break;
            }
        }
        let covered = ByteSpan {
            start: begin,
            len: cursor.saturating_sub(begin),
        };
        let reached = (first + lines) as usize;
        let next = (cursor < total).then(|| {
            cursor::issue(
                binding.scope,
                binding.epoch,
                Operation::Read,
                binding.request,
                0,
                reached,
                cursor,
            )
        });
        let content = if text.is_empty() && covered.len > 0 {
            ResultContent::Opaque { bytes: covered.len }
        } else {
            ResultContent::Text { text }
        };
        Ok(ResultRead {
            id: item.entry.id.clone(),
            encoding: item.entry.encoding,
            content,
            covered,
            total: Some(total),
            completeness: item.entry.completeness,
            next,
            line: Some(first),
        })
    }

    /// The line the scan reached and the stored offset it begins at. A mark
    /// covers the line when one was recorded; otherwise the scan restarts from
    /// the latest mark at or before it, which is why only every stride-th line
    /// is kept. A mark past the line asked for is never the answer: starting
    /// from it would skip the line the caller wants.
    async fn locate_line(
        &self,
        item: &IndexedResult,
        start: u64,
    ) -> Result<(u64, u64), ResultCatalogError> {
        let (mut line, mut offset) = {
            let marks = item.marks.lock().unwrap_or_else(PoisonError::into_inner);
            match marks.get(start as usize) {
                Some(found) => (start, found),
                None => match marks.at_or_before(start as usize) {
                    Some((at, found)) => (at as u64, found),
                    // Line one begins at the first stored byte.
                    _ => (1, 0),
                },
            }
        };
        let mut scanned = 0u64;
        while line < start {
            let window =
                windows::read_window(self.backend(), &item.source, offset, WINDOW_BYTES).await?;
            if window.end <= offset {
                return Ok((line, offset));
            }
            scanned += window.end - window.start;
            if scanned > LINE_SCAN_BYTES {
                return Err(ResultCatalogError::InvalidRange(format!(
                    "line {start} is further than one read scans; read it by byte offset"
                )));
            }
            let mut marks = item.marks.lock().unwrap_or_else(PoisonError::into_inner);
            for (at, ch) in window.text.char_indices() {
                if ch != '\n' {
                    continue;
                }
                line += 1;
                let stored = window.stored_offset(at + 1);
                marks.record(line as usize, stored);
                if line == start {
                    return Ok((line, stored));
                }
            }
            offset = window.end;
        }
        Ok((line, offset))
    }

    /// Collect the aggregate over everything the index holds.
    fn aggregate(index: &Index) -> ResultStats {
        let mut events = HashSet::new();
        let mut stats = ResultStats {
            results: 0,
            streams: index.results.len(),
            complete_streams: 0,
            truncated_streams: 0,
            unknown_streams: 0,
            corrupt: index.corrupt,
            skipped_bytes: index.skipped_bytes,
            stdout_bytes: 0,
            stderr_bytes: 0,
            unmeasured_streams: 0,
            oldest_ms: None,
            newest_ms: None,
            as_of: index.as_of.clone(),
            complete: index.covers_whole_log(),
        };
        for result in &index.results {
            events.insert(result.entry.event.clone());
            match result.entry.completeness {
                ResultCompleteness::Complete => stats.complete_streams += 1,
                ResultCompleteness::Truncated => stats.truncated_streams += 1,
                _ => stats.unknown_streams += 1,
            }
            match result.entry.stream {
                ResultStream::Stdout => {
                    stats.stdout_bytes += result.entry.bytes.unwrap_or(0);
                }
                ResultStream::Stderr => {
                    stats.stderr_bytes += result.entry.bytes.unwrap_or(0);
                }
                _ => {}
            }
            if result.entry.bytes.is_none() {
                // Only a stored field reaches this: a captured stream always
                // states its size. Its bytes are not in the totals, so the
                // count says how much the figures leave out.
                stats.unmeasured_streams += 1;
            }
            if let Some(at) = result.entry.created_ms {
                stats.oldest_ms = Some(stats.oldest_ms.map_or(at, |old| old.min(at)));
                stats.newest_ms = Some(stats.newest_ms.map_or(at, |new| new.max(at)));
            }
        }
        stats.results = events.len();
        stats
    }
}

impl ResultCatalog for SessionResultCatalog {
    fn list(
        &self,
        scope: SessionId,
        query: ResultListQuery,
    ) -> PFut<'_, Result<ResultPage, ResultCatalogError>> {
        Box::pin(async move {
            let mut indexes = self.indexes.lock().await;
            let index = indexes.entry(scope).or_default();
            fold::refresh(self.log.as_ref(), scope, index).await?;
            let limit = query
                .limit
                .map_or(DEFAULT_PAGE, |limit| limit as usize)
                .clamp(1, DEFAULT_PAGE);
            let request = cursor::request(&[&filter_key(&query.filter)]);
            let epoch = index.epoch.clone().unwrap_or_default();
            let start = match &query.cursor {
                Some(at) => {
                    cursor::parse(
                        at,
                        scope,
                        &epoch,
                        Operation::List,
                        request,
                        index.results.len(),
                    )?
                    .entry
                }
                None => 0,
            };
            let mut entries = Vec::new();
            let mut at = start;
            while at < index.results.len() && entries.len() < limit {
                let row = &index.results[at].entry;
                if matches_filter(row, &query.filter) {
                    entries.push(row.clone());
                }
                at += 1;
            }
            let next = (at < index.results.len()).then(|| {
                cursor::issue(
                    scope,
                    &epoch,
                    Operation::List,
                    request,
                    index.results.len(),
                    at,
                    0,
                )
            });
            Ok(ResultPage {
                entries,
                next,
                complete: index.covers_whole_log(),
            })
        })
    }

    fn search(
        &self,
        scope: SessionId,
        query: ResultSearchQuery,
    ) -> PFut<'_, Result<ResultSearchPage, ResultCatalogError>> {
        Box::pin(async move {
            let within = query.within.as_ref().map(|id| id.as_str().to_string());
            let request = cursor::request(&[&query.query, within.as_deref().unwrap_or("")]);
            let (targets, from, offset, watermark, complete, epoch) = {
                let mut indexes = self.indexes.lock().await;
                let index = indexes.entry(scope).or_default();
                fold::refresh(self.log.as_ref(), scope, index).await?;
                let length = index.results.len();
                let epoch = index.epoch.clone().unwrap_or_default();
                let (from, offset) = match &query.cursor {
                    Some(at) => {
                        let decoded =
                            cursor::parse(at, scope, &epoch, Operation::Search, request, length)?;
                        (decoded.entry, decoded.offset)
                    }
                    None => (0, 0),
                };
                let targets = index
                    .results
                    .iter()
                    .filter(|result| {
                        query
                            .within
                            .as_ref()
                            .is_none_or(|id| result.entry.id == *id)
                    })
                    .map(|result| Target {
                        id: result.entry.id.clone(),
                        source: result.source.clone(),
                    })
                    .collect::<Vec<_>>();
                let complete = index.covers_whole_log();
                (targets, from, offset, length, complete, epoch)
            };
            if query.within.is_some() && targets.is_empty() {
                return Err(ResultCatalogError::NotFound);
            }
            let found = search::scan(
                self.backend(),
                &query.query,
                &targets,
                from,
                offset,
                scan_budget(query.budget),
                MAX_SEARCH_HITS as usize,
            )
            .await?;
            let next = found.next.map(|(entry, offset)| {
                cursor::issue(
                    scope,
                    &epoch,
                    Operation::Search,
                    request,
                    watermark,
                    entry,
                    offset,
                )
            });
            Ok(ResultSearchPage {
                hits: found.hits,
                scanned_bytes: found.scanned_bytes,
                partial: found.partial,
                complete,
                next,
            })
        })
    }

    fn read(
        &self,
        scope: SessionId,
        query: ResultReadQuery,
    ) -> PFut<'_, Result<ResultRead, ResultCatalogError>> {
        Box::pin(async move {
            let (item, epoch) = {
                let mut indexes = self.indexes.lock().await;
                let index = indexes.entry(scope).or_default();
                fold::refresh(self.log.as_ref(), scope, index).await?;
                let at = index
                    .lookup(query.id.as_str())
                    .ok_or(ResultCatalogError::NotFound)?;
                let item = index.results[at].clone();
                let epoch = index.epoch.clone().unwrap_or_default();
                (item, epoch)
            };
            let binding = CursorBinding {
                scope,
                epoch: &epoch,
                request: 0,
            };
            self.serve_read(binding, item, query).await
        })
    }

    fn stats(&self, scope: SessionId) -> PFut<'_, Result<ResultStats, ResultCatalogError>> {
        Box::pin(async move {
            let mut indexes = self.indexes.lock().await;
            let index = indexes.entry(scope).or_default();
            fold::refresh(self.log.as_ref(), scope, index).await?;
            Ok(Self::aggregate(index))
        })
    }
}

/// The budget a scan runs under, bounded by the host's own ceiling so a caller
/// cannot ask one request to read the whole store.
fn scan_budget(asked: Option<ScanBudget>) -> ScanBudget {
    let asked = asked.unwrap_or(ScanBudget {
        max_bytes: DEFAULT_SCAN_BYTES,
        max_millis: DEFAULT_SCAN_MILLIS,
    });
    ScanBudget {
        max_bytes: asked.max_bytes.clamp(1, DEFAULT_SCAN_BYTES),
        max_millis: asked.max_millis.clamp(1, DEFAULT_SCAN_MILLIS),
    }
}

/// Whether a listing row answers the filter.
fn matches_filter(entry: &ResultEntry, filter: &ResultFilter) -> bool {
    if let Some(tool) = &filter.tool
        && entry.tool.as_deref() != Some(tool.as_str())
    {
        return false;
    }
    if let Some(since) = filter.since_ms
        && entry.created_ms.is_none_or(|at| at < since)
    {
        return false;
    }
    if let Some(until) = filter.until_ms
        && entry.created_ms.is_none_or(|at| at > until)
    {
        return false;
    }
    if let Some(min) = filter.min_bytes
        && entry.bytes.is_none_or(|bytes| bytes < min)
    {
        return false;
    }
    true
}

/// The filter as one string a cursor binds to. Two listings that would return
/// different rows must not accept each other's cursors.
fn filter_key(filter: &ResultFilter) -> String {
    format!(
        "{}|{:?}|{:?}|{:?}",
        filter.tool.as_deref().unwrap_or(""),
        filter.since_ms,
        filter.until_ms,
        filter.min_bytes
    )
}

/// Turn one decoded window into the answer a read returns.
fn assemble_read(
    binding: CursorBinding<'_>,
    item: &IndexedResult,
    window: DecodedWindow,
    line: Option<u64>,
) -> ResultRead {
    let covered = ByteSpan {
        start: window.start,
        len: window.end.saturating_sub(window.start),
    };
    let content = if window.text.is_empty() && covered.len > 0 {
        ResultContent::Opaque { bytes: covered.len }
    } else {
        ResultContent::Text { text: window.text }
    };
    let next = (window.end < window.total).then(|| {
        cursor::issue(
            binding.scope,
            binding.epoch,
            Operation::Read,
            binding.request,
            0,
            0,
            window.end,
        )
    });
    ResultRead {
        id: item.entry.id.clone(),
        encoding: item.entry.encoding,
        content,
        covered,
        total: Some(window.total),
        completeness: item.entry.completeness,
        next,
        line,
    }
}
