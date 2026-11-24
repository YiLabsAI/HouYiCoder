//! Records successful memory mutations within one background pass.

use std::sync::Mutex;

use houyicoder_api::agent_event::{MemoryChange, MemoryOperation};
use houyicoder_context::MemoryScope;

/// Records successful memory mutations and atomically drains them.
#[derive(Default)]
pub(crate) struct MutationLog {
    changes: Mutex<Vec<MemoryChange>>,
}

impl MutationLog {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Append one successful mutation, with the scope it was addressed to so
    /// the notice can say where the memory went and not only which key moved.
    pub(crate) fn record(
        &self,
        key: impl Into<String>,
        operation: MemoryOperation,
        scope: MemoryScope,
    ) {
        self.changes
            .lock()
            .expect("mutation log lock")
            .push(MemoryChange {
                key: key.into(),
                operation,
                scope,
            });
    }

    /// Atomically take every recorded mutation and leave the log empty.
    pub(crate) fn take(&self) -> Vec<MemoryChange> {
        std::mem::take(&mut *self.changes.lock().expect("mutation log lock"))
    }

    /// Append a batch of already-recorded mutations in order.
    pub(crate) fn extend(&self, more: impl IntoIterator<Item = MemoryChange>) {
        self.changes.lock().expect("mutation log lock").extend(more);
    }

    /// Atomically take and collapse mutations by key so one pipeline notice
    /// lists each touched key at most once with its net operation and scope.
    pub(crate) fn take_merged(&self) -> Vec<MemoryChange> {
        let raw = self.take();
        let mut merged: Vec<MemoryChange> = Vec::with_capacity(raw.len());
        for change in raw {
            if let Some(pos) = merged.iter().position(|m| m.key == change.key) {
                let prior = &merged[pos];
                if prior.operation == MemoryOperation::Created
                    && change.operation == MemoryOperation::Deleted
                    && prior.scope == change.scope
                {
                    merged.remove(pos);
                } else if prior.operation == MemoryOperation::Created
                    && change.operation == MemoryOperation::Updated
                    && prior.scope == change.scope
                {
                    // Created then refreshed in the same pass stays Created.
                } else {
                    merged[pos] = change;
                }
            } else {
                merged.push(change);
            }
        }
        merged
    }
}

#[cfg(test)]
mod tests;
