//! Background fill of legacy session prompts from their logs.
//!
//! A record written before prompts were recorded states no title, and the
//! session listing answers from records alone. This pass reads a bounded page
//! of session directories per launch, fills each unrecorded session from a
//! fixed head and tail window of its log, and advances a durable cursor, so a
//! store larger than one page migrates over several launches instead of
//! rescanning the same newest sessions every time.

use std::fs::create_dir_all;
use std::fs::read;
use std::fs::rename;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use houyicoder_api::session::SessionLog;
use houyicoder_context::PromptCoverage;
use houyicoder_context::SessionDescriptorStore;
use houyicoder_context::SessionId;
use houyicoder_context::session_class::PageCursor;
use houyicoder_context::session_class::SessionEntry;
use houyicoder_context::session_class::session_page;
use houyicoder_service::lifecycle::prompt_backfill::ScanOutcome;
use houyicoder_service::lifecycle::prompt_backfill::backfill_session;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::spawn_blocking;
use tokio::time::sleep;

/// Seconds to wait before the pass starts, so the first frame and the session
/// load do not compete with the scan for the disk.
const START_DELAY_SECS: u64 = 2;

/// Session directories one launch walks. The page is the unit the cursor
/// advances by, so it bounds the descriptor reads a launch pays whatever the
/// store's size.
const PAGE_SIZE: usize = 400;

/// Sessions one launch scans, after the page has been filtered to those whose
/// record states nothing.
const SCAN_LIMIT: usize = 200;

/// Log bytes one launch reads. The session limit alone would allow a page of
/// full windows, so this ceiling binds first on a store of long logs and keeps
/// one launch's disk work small.
const BYTE_BUDGET: u64 = 16 * 1024 * 1024;

/// The file holding the cursor, under the config home.
const CURSOR_FILE: &str = ".prompt-backfill-cursor";

/// The three bounds one pass runs under. Each is a data field rather than a
/// constant read inside the pass, so a test can drive a small store through
/// the same code the launch path runs.
#[derive(Debug, Clone, Copy)]
pub struct PassBounds {
    /// Session directories one pass walks.
    pub page: usize,
    /// Sessions one pass scans.
    pub sessions: usize,
    /// Log bytes one pass reads.
    pub bytes: u64,
}

impl Default for PassBounds {
    fn default() -> Self {
        Self {
            page: PAGE_SIZE,
            sessions: SCAN_LIMIT,
            bytes: BYTE_BUDGET,
        }
    }
}

/// What one pass over the store did, and where the next one continues.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillReport {
    /// Session directories the pass looked at.
    pub examined: usize,
    /// Sessions whose log it scanned.
    pub scanned: usize,
    /// Log bytes those scans read.
    pub bytes_read: u64,
    /// The cursor the next pass continues below. None once the pass reached
    /// the oldest session, which restarts the walk at the newest one.
    pub next_cursor: Option<PageCursor>,
}

/// Scan one page of the store, starting below a cursor.
///
/// The pass stops at the first of three bounds: the page, the session limit,
/// or the byte budget, and a cancelled pass stops where it is. Either way the
/// cursor it returns points at the last session it examined, so the work it
/// did not reach is the first thing the next pass does.
pub fn run_pass(
    sessions_root: &Path,
    log: &dyn SessionLog,
    store: &dyn SessionDescriptorStore,
    before: Option<PageCursor>,
    bounds: PassBounds,
    cancelled: &AtomicBool,
) -> BackfillReport {
    let page = session_page(sessions_root, before, bounds.page);
    let mut report = BackfillReport::default();
    for entry in &page {
        if cancelled.load(Ordering::Relaxed)
            || report.scanned >= bounds.sessions
            || report.bytes_read >= bounds.bytes
        {
            break;
        }
        report.examined += 1;
        if needs_backfill(entry) {
            match backfill_session(log, store, entry.sid, cancelled) {
                Ok(ScanOutcome::Scanned(scan)) => {
                    report.scanned += 1;
                    report.bytes_read += scan.bytes_read;
                }
                // The scan stopped partway through this session's windows, so
                // the cursor stays above it: the next pass retries the session
                // the cancel interrupted rather than skipping it.
                Ok(ScanOutcome::Cancelled) => break,
                Ok(ScanOutcome::Unsupported) => {}
                Err(error) => tracing::warn!(
                    session = %entry.sid,
                    "prompt backfill: could not write the record: {error}"
                ),
            }
        }
        // Advance only after the session is handled. A cursor moved before the
        // work leaves an interrupted session behind it, where no later pass
        // reaches it.
        report.next_cursor = Some(PageCursor {
            last_active: entry.last_active,
            sid: entry.sid,
        });
    }
    if page.len() < bounds.page {
        // The page was short, so the walk reached the oldest session and the
        // next pass starts over from the newest.
        report.next_cursor = None;
    }
    report
}

/// Whether a session is one the pass scans: one of the user's own sessions,
/// holding a log, whose record states no prompt.
///
/// A record claiming unknown coverage was never read, so it is due. A record
/// claiming whole-log coverage but stating no prompt is due on every walk
/// because two histories produce that record and the record cannot tell them
/// apart: the session's recorder judged that no input states a task, or its
/// write of the first statement failed after the record was created, leaving
/// the created record standing. Re-reading an empty session costs one bounded
/// read of both windows and writes the same judgment back; missing a failed
/// write would leave a titled session untitled for good.
fn needs_backfill(entry: &SessionEntry) -> bool {
    entry.is_user_session()
        && entry.descriptor.as_ref().is_some_and(|descriptor| {
            matches!(
                descriptor.prompt_coverage,
                PromptCoverage::Unknown | PromptCoverage::Complete
            ) && descriptor.first_prompt.is_none()
                && descriptor.last_prompt.is_none()
        })
}

/// Run one pass and persist its cursor, then report what it did.
///
/// Call from inside a spawned task so cancellation can stop it: the guard
/// sets the flag when the task is dropped, and a scan reads the flag between
/// records rather than finishing a page nobody is waiting for.
pub async fn run_background(
    sessions_root: PathBuf,
    log: Arc<dyn SessionLog>,
    store: Arc<dyn SessionDescriptorStore>,
) {
    sleep(Duration::from_secs(START_DELAY_SECS)).await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _stop = CancelOnDrop(cancelled.clone());
    let handled = spawn_blocking(move || {
        let before = read_cursor(&sessions_root);
        let report = run_pass(
            &sessions_root,
            log.as_ref(),
            store.as_ref(),
            before,
            PassBounds::default(),
            &cancelled,
        );
        write_cursor(&sessions_root, report.next_cursor);
        report
    })
    .await;
    match handled {
        Ok(report) if report.scanned > 0 => tracing::info!(
            "prompt backfill: scanned {} of {} sessions, {} bytes",
            report.scanned,
            report.examined,
            report.bytes_read
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!("prompt backfill: the pass did not finish: {error}"),
    }
}

/// Stops a running pass when its task is dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The cursor as stored on disk. It carries no version: a file this build
/// cannot read parses to nothing, and the walk restarts at the newest session,
/// which costs one page and loses nothing. Living under the sessions root, it
/// answers for one store rather than for the config home that points at it.
#[derive(Serialize, Deserialize)]
struct Cursor {
    before: Option<StoredPage>,
}

/// One page boundary on disk: the activity second and the id, the two values
/// the walk orders by.
#[derive(Serialize, Deserialize)]
struct StoredPage {
    last_active: u64,
    sid: String,
}

fn cursor_path(root: &Path) -> PathBuf {
    root.join(CURSOR_FILE)
}

fn read_cursor(root: &Path) -> Option<PageCursor> {
    let bytes = read(cursor_path(root)).ok()?;
    let cursor: Cursor = serde_json::from_slice(&bytes).ok()?;
    let page = cursor.before?;
    Some(PageCursor {
        last_active: page.last_active,
        sid: SessionId::from_display_string(&page.sid)?,
    })
}

/// Write the cursor through a per-writer temp file and a rename, so a reader
/// never parses a half-written file and two launches cannot truncate each
/// other's bytes.
fn write_cursor(root: &Path, before: Option<PageCursor>) {
    let cursor = Cursor {
        before: before.map(|page| StoredPage {
            last_active: page.last_active,
            sid: page.sid.to_string(),
        }),
    };
    let Ok(bytes) = serde_json::to_vec(&cursor) else {
        return;
    };
    // The store root may not exist yet on a fresh install, and the cursor is
    // the only state this pass keeps: create it rather than losing the walk.
    let _created = create_dir_all(root);
    let tmp = root.join(format!("{CURSOR_FILE}.{}.tmp", id()));
    if write(&tmp, bytes).is_err() || rename(&tmp, cursor_path(root)).is_err() {
        tracing::warn!("prompt backfill: could not write the cursor");
    }
}

#[cfg(test)]
mod tests;
