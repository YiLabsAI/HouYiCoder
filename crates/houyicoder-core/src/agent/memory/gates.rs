//! Runtime memory feature gates.
//!
//! Independent atomics control recall, extraction, and consolidation without
//! locking the agent loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Snapshot of the memory feature gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryGateState {
    pub auto_memory: bool,
    pub auto_dream: bool,
}

/// Runtime switches for automatic memory behavior, shared via Arc so delegated
/// children see live toggle flips made in the parent session.
#[derive(Clone)]
pub struct MemoryGates {
    auto_memory: Arc<AtomicBool>,
    auto_dream: Arc<AtomicBool>,
}

impl MemoryGates {
    pub fn new(auto_memory: bool, auto_dream: bool) -> Self {
        Self {
            auto_memory: Arc::new(AtomicBool::new(auto_memory)),
            auto_dream: Arc::new(AtomicBool::new(auto_dream)),
        }
    }

    pub(crate) fn state(&self) -> MemoryGateState {
        MemoryGateState {
            auto_memory: self.auto_memory.load(Ordering::Relaxed),
            auto_dream: self.auto_dream.load(Ordering::Relaxed),
        }
    }

    /// Whether automatic memory recall and maintenance are enabled.
    pub fn auto_memory_enabled(&self) -> bool {
        self.auto_memory.load(Ordering::Relaxed)
    }

    pub(crate) fn set_auto_memory(&self, enabled: bool) {
        self.auto_memory.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn auto_dream_enabled(&self) -> bool {
        self.auto_dream.load(Ordering::Relaxed)
    }

    pub(crate) fn set_auto_dream(&self, enabled: bool) {
        self.auto_dream.store(enabled, Ordering::Relaxed);
    }
}
