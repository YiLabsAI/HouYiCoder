//! Append-only recording of successful memory operations within one pass.

use std::sync::Mutex;

use houyicoder_api::agent_event::{MemoryChange, MemoryOperation};

/// Records successful memory changes and atomically drains them.
#[derive(Default)]
pub(crate) struct MemoryChangeRecorder {
    changes: Mutex<Vec<MemoryChange>>,
}

impl MemoryChangeRecorder {
    /// Create an empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one successful operation.
    pub fn record(&self, key: impl Into<String>, operation: MemoryOperation) {
        self.changes
            .lock()
            .expect("memory change recorder lock")
            .push(MemoryChange {
                key: key.into(),
                operation,
            });
    }

    /// Atomically take every recorded operation and leave the recorder empty.
    pub fn take(&self) -> Vec<MemoryChange> {
        std::mem::take(&mut *self.changes.lock().expect("memory change recorder lock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recorder_drains_successful_changes() {
        let recorder = MemoryChangeRecorder::new();
        recorder.record("key", MemoryOperation::Stored);
        assert_eq!(recorder.take().len(), 1);
        assert!(recorder.take().is_empty());
    }
}
