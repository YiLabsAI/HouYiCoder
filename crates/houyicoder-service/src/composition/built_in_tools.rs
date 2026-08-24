//! The built-in tool provider: the composition root's own ToolProvider
//! implementation, assembling the sandboxed filesystem + search + network
//! tools through the permission gate. Split from the composition root so the
//! root file stays under the size gate.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::{Tool, ToolProvider};
use houyicoder_core::agent::{
    AskUserQuestionTool, BashTool, EditTool, GlobTool, GrepTool, MultiEditTool, ReadTool,
    WebFetchTool, WriteTool,
};
use houyicoder_core::snapshot::{SnapshotStore, UndoStack};
use houyicoder_permission::{GuardedTool, ModeGate};

/// The built-in tool set: AskUserQuestion always, plus the sandboxed
/// filesystem + search + network tools (wrapped through the permission
/// gate) when a sandbox is present. One ToolProvider so the composition
/// root assembles its tools the same way it assembles any external
/// provider's. Adding a built-in tool is editing this provider; adding an
/// external tool set is adding a different provider, not touching this one.
pub(super) struct BuiltInToolProvider {
    session: Option<Arc<dyn SandboxSession>>,
    gate: Arc<dyn ModeGate>,
    undo_stack: Option<Arc<std::sync::Mutex<UndoStack>>>,
    snapshot_store: Option<Arc<SnapshotStore>>,
}

impl BuiltInToolProvider {
    pub(super) fn new(session: Option<Arc<dyn SandboxSession>>, gate: Arc<dyn ModeGate>) -> Self {
        let (undo_stack, snapshot_store) = match &session {
            Some(s) => {
                let store = super::degrade_with_notice(
                    SnapshotStore::new(s.workspace_root()),
                    "snapshot store init failed",
                    "undo unavailable; destructive bash commands will require explicit approval",
                )
                .map(Arc::new);
                // Re-link the undo stack to surviving on-disk snapshots so a
                // resumed session can /undo a destructive op from the prior
                // process: the in-memory stack is lost on restart, but the
                // snap-N dirs persist. An empty stack (store init failed, or
                // no surviving snapshots) leaves /undo as no-op, same as before.
                let stack = Arc::new(std::sync::Mutex::new(match &store {
                    Some(st) => UndoStack::from_entries(st.relink_undo_entries()),
                    None => UndoStack::new(),
                }));
                (Some(stack), store)
            }
            None => (None, None),
        };
        Self {
            session,
            gate,
            undo_stack,
            snapshot_store,
        }
    }

    /// The shared undo stack (cloned into the BashTool; also set on the Runner
    /// for /undo). None when no session or the snapshot store failed.
    pub(super) fn undo_handles(
        &self,
    ) -> Option<(Arc<std::sync::Mutex<UndoStack>>, Arc<SnapshotStore>)> {
        self.undo_stack
            .as_ref()
            .zip(self.snapshot_store.as_ref())
            .map(|(s, st)| (s.clone(), st.clone()))
    }
}

impl ToolProvider for BuiltInToolProvider {
    fn name(&self) -> &str {
        "builtin"
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut v: Vec<Arc<dyn Tool>> = vec![Arc::new(AskUserQuestionTool::new())];
        if let Some(session) = &self.session {
            let bash = match (&self.undo_stack, &self.snapshot_store) {
                (Some(stack), Some(store)) => {
                    BashTool::with_undo(session.clone(), stack.clone(), store.clone())
                }
                _ => BashTool::new(session.clone()),
            };
            v.push(Arc::new(GuardedTool::new(
                Arc::new(bash),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(ReadTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(WriteTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(EditTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(MultiEditTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(GlobTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(GrepTool::new(session.clone())),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(WebFetchTool::new()),
                self.gate.clone(),
            )));
        }
        v
    }
}
