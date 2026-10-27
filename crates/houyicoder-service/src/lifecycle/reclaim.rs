//! Block reclamation for the sessions store: free the content-addressed
//! blocks no reference names.
//!
//! The pass keeps every block a live reference names, a block a writer
//! stored but has not yet referenced, and everything on a store whose sources
//! cannot be read in full. A pass stopped while marking deletes nothing; one
//! stopped while sweeping keeps what its finished shards freed and reports
//! that count. The pass lives in the memory crate, beside the store it walks;
//! this module is the lifecycle entry point a maintenance caller reaches it
//! through.

use std::path::Path;

use houyicoder_memory::LocalFileBackend;

pub use houyicoder_memory::ReclaimError;
pub use houyicoder_memory::ReclaimPolicy;
pub use houyicoder_memory::ReclaimReport;

/// What one reclaim pass did. The four endings are the whole contract: a
/// caller that gains a branch here has to decide what to report, so the type
/// stays exhaustive and a new ending fails to compile at every reader.
#[derive(Debug)]
pub enum ReclaimOutcome {
    /// The pass walked every shard and freed what nothing named.
    Completed(ReclaimReport),
    /// The pass freed what nothing named in the shards it reached, and the
    /// budget stopped it before the rest. The next window continues.
    Partial(ReclaimReport),
    /// Another process holds the store's reclaim lock, so this pass did
    /// nothing. The next maintenance window tries again.
    Skipped,
    /// The pass stopped before finishing: a source could not be read, its
    /// workspace could not be written, the budget ran out while the mark was
    /// still reading sources, or the source list changed under the mark. The
    /// report says what it freed before it stopped, which is nothing unless
    /// the sweep had already begun.
    Stopped {
        error: ReclaimError,
        report: ReclaimReport,
    },
}

/// Reclaim the blocks no reference in the store names.
///
/// The policy is the caller's: it decides how long a block stays protected
/// for a writer that has not yet appended its reference, and how long this
/// pass may run before it leaves the rest to the next one.
pub fn reclaim_store_blocks(root: &Path, policy: &ReclaimPolicy) -> ReclaimOutcome {
    let backend = LocalFileBackend::new(root.to_path_buf());
    match backend.reclaim_orphan_blocks(policy) {
        Ok(report) if report.complete => ReclaimOutcome::Completed(report),
        Ok(report) => ReclaimOutcome::Partial(report),
        Err(stop) if matches!(stop.error, ReclaimError::Busy) => ReclaimOutcome::Skipped,
        Err(stop) => ReclaimOutcome::Stopped {
            error: stop.error,
            report: stop.report,
        },
    }
}

#[cfg(test)]
mod tests;
