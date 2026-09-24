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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_drains_successful_changes() {
        let log = MutationLog::new();
        log.record("key", MemoryOperation::Created, MemoryScope::Auto);
        assert_eq!(log.take().len(), 1);
        assert!(log.take().is_empty());
    }
}
