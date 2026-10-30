//! The stored-result request the recall tool answers: list what a tool kept,
//! read a window of one stream, search inside the kept bytes, or report the
//! session's totals. Every answer is bounded, so a recall cannot re-inject the
//! block it was meant to replace.

use houyicoder_api::result_catalog::{ResultCatalog, ResultCatalogError};
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::result_catalog::{
    ResultCompleteness, ResultContent, ResultCursor, ResultEntry, ResultFilter, ResultId,
    ResultListQuery, ResultRange, ResultRead, ResultReadQuery, ResultSearchPage, ResultSearchQuery,
    ResultStats, ResultStream,
};
use serde::Deserialize;

use crate::agent::tools::results::SessionResultCatalog;

/// Bytes one read returns when the caller asks for no particular length.
const READ_BYTES: u32 = 4096;

/// The most bytes one read returns.
const READ_BYTES_MAX: u32 = 8000;

/// Lines one read returns when the caller names a line but no count.
const DEFAULT_LINES: u32 = 40;

/// What a caller asks of the stored output blocks.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(super) enum Request {
    /// List what was kept, oldest first.
    List {
        #[serde(default)]
        tool: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Search inside one result, or inside every result.
    Search {
        query: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Read one window of one stream.
    Read {
        id: String,
        #[serde(default)]
        offset: Option<u64>,
        #[serde(default)]
        len: Option<u32>,
        #[serde(default)]
        line: Option<u64>,
        #[serde(default)]
        count: Option<u32>,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Report what the session kept in total.
    Stats,
}

/// Answer one stored-result request from the catalog.
pub(super) async fn serve(
    catalog: &SessionResultCatalog,
    session: SessionId,
    request: Request,
) -> Result<String, ToolError> {
    match request {
        Request::List {
            tool,
            cursor,
            limit,
        } => {
            let filtered = tool.is_some();
            let page = catalog
                .list(
                    session,
                    ResultListQuery {
                        filter: ResultFilter {
                            tool,
                            ..ResultFilter::default()
                        },
                        cursor: cursor.map(ResultCursor::new),
                        limit,
                    },
                )
                .await
                .map_err(catalog_error)?;
            Ok(render_list(
                &page.entries,
                page.next.as_ref(),
                page.complete,
                filtered,
            ))
        }
        Request::Search { query, id, cursor } => {
            let found = catalog
                .search(
                    session,
                    ResultSearchQuery {
                        query,
                        within: id.map(ResultId::new),
                        cursor: cursor.map(ResultCursor::new),
                        budget: None,
                    },
                )
                .await
                .map_err(catalog_error)?;
            Ok(render_search(&found))
        }
        Request::Read {
            id,
            offset,
            len,
            line,
            count,
            cursor,
        } => {
            let range = match line {
                Some(start) => ResultRange::Lines {
                    start,
                    count: count.unwrap_or(DEFAULT_LINES),
                },
                None => ResultRange::Bytes {
                    offset: offset.unwrap_or(0),
                    len: len.unwrap_or(READ_BYTES).min(READ_BYTES_MAX),
                },
            };
            let read = catalog
                .read(
                    session,
                    ResultReadQuery {
                        id: ResultId::new(id),
                        range,
                        cursor: cursor.map(ResultCursor::new),
                    },
                )
                .await
                .map_err(catalog_error)?;
            Ok(render_read(&read))
        }
        Request::Stats => {
            let stats = catalog.stats(session).await.map_err(catalog_error)?;
            Ok(render_stats(&stats))
        }
    }
}

/// Map a catalog failure onto the answer a tool caller understands.
fn catalog_error(error: ResultCatalogError) -> ToolError {
    ToolError::Failed(format!("conversation_search: {error}"))
}

fn render_list(
    entries: &[ResultEntry],
    next: Option<&ResultCursor>,
    complete: bool,
    filtered: bool,
) -> String {
    if entries.is_empty() {
        // Only an unfiltered listing over a whole log answers that nothing was
        // kept. A filter that matched nothing, or a log the fold could not
        // read whole, leaves rows the reader has not seen.
        return match (complete, filtered) {
            (true, true) => {
                "## Stored results\n\nNothing this session kept matches that filter.\n".to_string()
            }
            (true, false) => {
                "## Stored results\n\nNothing is kept beyond what the transcript shows.\n"
                    .to_string()
            }
            (false, _) => {
                "## Stored results\n\nNothing is kept in the part of the log this session could read; some of it was not read.\n"
                    .to_string()
            }
        };
    }
    let mut out = format!("## Stored results ({})\n\n", entries.len());
    for entry in entries {
        out.push_str(&format!(
            "- {} | {} | {} | {} | {}",
            entry.id.as_str(),
            entry.tool.as_deref().unwrap_or("unknown tool"),
            stream_name(&entry.stream),
            match entry.bytes {
                Some(bytes) => format!("{bytes} B"),
                None => "size unknown".to_string(),
            },
            state_name(entry),
        ));
        if let Some(command) = &entry.command {
            out.push_str(&format!(" | command: {command}"));
        }
        out.push('\n');
    }
    if !complete {
        // The page can be the whole of what the fold read and still be
        // partial: records the fold could not read leave nothing to page
        // through, and saying more rows follow would name rows that do not
        // exist.
        out.push_str(if next.is_some() {
            "\nThe log holds more than this page covers.\n"
        } else {
            "\nPart of the log was not read, so results kept there are not listed.\n"
        });
    }
    if let Some(cursor) = next {
        out.push_str(&format!("\nContinue with cursor: {}\n", cursor.as_str()));
        // The cursor is bound to the listing that issued it, so the answer
        // says which inputs to repeat with it.
        if filtered {
            out.push_str("Repeat the same tool filter with this cursor to continue.\n");
        }
    }
    out
}

/// The longest prefix of a text that fits a byte limit, cut on a character
/// boundary, with a note when it was cut. The limit is bytes because that is
/// what the window the caller asked for counts in.
fn cut_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... (truncated)", &text[..end])
}

fn render_read(read: &ResultRead) -> String {
    let mut out = match read.line {
        Some(line) => format!(
            "## Result {} (from line {line}, bytes {}-{} of {})\n\n",
            read.id.as_str(),
            read.covered.start,
            read.covered.end(),
            total_bytes(read),
        ),
        None => format!(
            "## Result {} (bytes {}-{} of {})\n\n",
            read.id.as_str(),
            read.covered.start,
            read.covered.end(),
            total_bytes(read),
        ),
    };
    match &read.content {
        ResultContent::Text { text } => out.push_str(&cut_bytes(text, READ_BYTES_MAX as usize)),
        ResultContent::Opaque { bytes } => out.push_str(&format!(
            "{bytes} bytes at this offset are not text and are not shown\n"
        )),
        _ => out.push_str("this build cannot show the stored content\n"),
    }
    if let Some(cursor) = &read.next {
        out.push_str(&format!("\n\nContinues with cursor: {}\n", cursor.as_str()));
        // A line read's cursor is only accepted by another line read, so
        // the answer says which inputs to repeat with it.
        if read.line.is_some() {
            out.push_str("Repeat this request with the same line and count to continue.\n");
        }
    }
    out
}

/// The stored length of a stream as a listing shows it.
fn total_bytes(read: &ResultRead) -> String {
    read.total
        .map_or("unknown".to_string(), |total| total.to_string())
}

fn render_search(page: &ResultSearchPage) -> String {
    if page.hits.is_empty() && !page.partial {
        // An index that covers part of the log answers only for the part it
        // read, so an empty page is not a report that nothing matches.
        return if page.complete {
            "## Stored results\n\nNo match in the stored output.\n".to_string()
        } else {
            "## Stored results\n\nNo match in the streams this session holds a complete record of; some of the log was not read.\n"
                .to_string()
        };
    }
    let mut out = format!("## Matches in stored output ({})\n\n", page.hits.len());
    for hit in &page.hits {
        match hit.line {
            Some(line) => out.push_str(&format!(
                "- {} line {line} (byte {}): {}\n",
                hit.id.as_str(),
                hit.span.start,
                hit.snippet
            )),
            None => out.push_str(&format!(
                "- {} at byte {}: {}\n",
                hit.id.as_str(),
                hit.span.start,
                hit.snippet
            )),
        }
    }
    if page.partial {
        out.push_str("\nThe scan stopped at its budget; more may match.\n");
    }
    if !page.complete {
        out.push_str("\nThe index covers part of the log, so hits in the rest are not listed.\n");
    }
    if let Some(cursor) = &page.next {
        out.push_str(&format!("Continue with cursor: {}\n", cursor.as_str()));
        out.push_str("Repeat the same query with this cursor to continue.\n");
    }
    out
}

fn render_stats(stats: &ResultStats) -> String {
    let mut out = format!(
        "## Stored results\n\n- results: {}\n- streams: {} ({} complete, {} truncated, {} unknown)\n",
        stats.results,
        stats.streams,
        stats.complete_streams,
        stats.truncated_streams,
        stats.unknown_streams,
    );
    out.push_str(&format!(
        "- stdout: {} B, stderr: {} B\n",
        stats.stdout_bytes, stats.stderr_bytes
    ));
    if stats.unmeasured_streams > 0 {
        let states = if stats.unmeasured_streams == 1 {
            "stream states"
        } else {
            "streams state"
        };
        out.push_str(&format!(
            "- {} stored {states} no size, so their bytes are not in the totals\n",
            stats.unmeasured_streams
        ));
    }
    if stats.corrupt > 0 || stats.skipped_bytes > 0 {
        out.push_str(&format!(
            "- unreadable: {} results, {} log bytes\n",
            stats.corrupt, stats.skipped_bytes
        ));
    }
    if let (Some(oldest), Some(newest)) = (stats.oldest_ms, stats.newest_ms) {
        out.push_str(&format!("- kept between {oldest} and {newest}\n"));
    }
    if !stats.complete {
        out.push_str("- the fold covers part of the log; the figures are a floor\n");
    }
    out
}

fn stream_name(stream: &ResultStream) -> String {
    match stream {
        ResultStream::Stdout => "stdout".to_string(),
        ResultStream::Stderr => "stderr".to_string(),
        ResultStream::Stored { field } if field.is_empty() => "whole output".to_string(),
        ResultStream::Stored { field } => format!("field {field}"),
        _ => "stream".to_string(),
    }
}

fn state_name(entry: &ResultEntry) -> String {
    // Completeness answers whether the kept bytes are whole, which is a
    // different question from how many bytes were kept, so the unknown value
    // does not borrow the wording of a missing size.
    let state = match entry.completeness {
        ResultCompleteness::Complete => "complete",
        ResultCompleteness::Truncated => "truncated",
        _ => "completeness unknown",
    };
    match entry.exit {
        Some(code) => format!("{state}, exit {code}"),
        None => state.to_string(),
    }
}

#[cfg(test)]
mod tests;
