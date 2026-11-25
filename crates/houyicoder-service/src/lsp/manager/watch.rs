//! Lifecycle helpers the manager uses but does not own: the guard that
//! restores a slot cancelled mid-handshake, and the suffix key one path
//! routes by.

use std::path::Path;
use std::sync::Arc;

use houyicoder_protocol::lsp::LspServerState;

use super::ServerSlot;

/// Restores a slot that was left Starting when the future holding it was
/// dropped. Best effort: the state lock is sync-restored only when it is
/// free, which it is unless another task is mid-write.
pub(super) struct StateGuard {
    pub(super) slot: Arc<ServerSlot>,
    pub(super) armed: bool,
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut state) = self.slot.state.try_write()
            && matches!(*state, LspServerState::Starting)
        {
            *state = LspServerState::Idle;
        }
    }
}

/// The lowercase suffix key of one path, with its leading dot.
pub(super) fn extension_of(path: &Path) -> String {
    format!(
        ".{}",
        path.extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    )
}
