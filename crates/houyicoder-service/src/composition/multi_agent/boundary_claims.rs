//! Claims deciding which writer closes one delegation boundary.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

/// The children whose return boundary a writer on this runtime has claimed.
///
/// A live ending and the restart scan can both reach one child, and only one
/// of them may write its return; the claim decides which before either
/// appends. The set is shared through an Arc because the scan runs on the
/// parent's turn while the live ending runs on the child's task.
#[derive(Clone, Default)]
pub(super) struct BoundaryClaims {
    held: Arc<Mutex<HashSet<String>>>,
}

impl BoundaryClaims {
    /// Claim the right to write this child's return boundary.
    ///
    /// None when another writer already holds it, in which case that writer
    /// owns the boundary and this caller writes nothing further about it.
    /// The guard releases the claim when it drops, so a writer cancelled
    /// mid-write leaves no claim that would outlive it and block the writer
    /// that finishes the child.
    pub(super) fn claim(&self, child: &str) -> Option<BoundaryClaim<'_>> {
        let mut held = self.held();
        held.insert(child.to_string()).then(|| BoundaryClaim {
            claims: self,
            child: child.to_string(),
        })
    }

    /// The held set, recovering the guard a panicking writer left poisoned.
    /// Reading a poisoned lock as a failed claim would read as "another
    /// writer owns it" and stop both writers for this child.
    fn held(&self) -> MutexGuard<'_, HashSet<String>> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One writer's claim on a child's return boundary, released on drop.
pub(super) struct BoundaryClaim<'a> {
    claims: &'a BoundaryClaims,
    child: String,
}

impl Drop for BoundaryClaim<'_> {
    fn drop(&mut self) {
        self.claims.held().remove(&self.child);
    }
}
