//! The built-in tool provider: the composition root's own ToolProvider
//! implementation, assembling the sandboxed filesystem + search + network
//! tools through the permission gate. Split from the composition root so the
//! root file stays under the size gate.

use std::path::Path;
use std::sync::Arc;

use super::paths::snapshot_store_root_with_home;
use houyicoder_api::background::BackgroundCommandLauncher;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::{Tool, ToolProvider};
use houyicoder_core::agent::ConditionalSkillActivator;
use houyicoder_core::agent::{
    AskUserQuestionTool, BashTool, EditTool, GlobTool, GrepTool, MultiEditTool, ReadTool,
    WebFetchTool, WriteTool,
};
use houyicoder_core::snapshot::file_transaction::FileTransaction;
use houyicoder_core::snapshot::{SnapshotStore, UndoStack};
use houyicoder_permission::{GuardedTool, ModeGate};
use std::sync::Mutex;
use std::time::Duration;

/// The built-in tool set: AskUserQuestion always, plus the sandboxed
/// filesystem + search + network tools (wrapped through the permission
/// gate) when a sandbox is present. One ToolProvider so the composition
/// root assembles its tools the same way it assembles any external
/// provider's. Adding a built-in tool is editing this provider; adding an
/// external tool set is adding a different provider, not touching this one.
pub(super) struct BuiltInToolProvider {
    session: Option<Arc<dyn SandboxSession>>,
    gate: Arc<dyn ModeGate>,
    undo_stack: Option<Arc<Mutex<UndoStack>>>,
    snapshot_store: Option<Arc<SnapshotStore>>,
    activator: Option<Arc<dyn ConditionalSkillActivator>>,
    active_skill: Option<Arc<Mutex<Option<String>>>>,
    transaction: Option<Arc<FileTransaction>>,
    /// The host that takes a shell command over once it outlives the
    /// foreground budget, and how long it may hold the foreground first.
    background: Option<(Arc<dyn BackgroundCommandLauncher>, Duration)>,
}

impl BuiltInToolProvider {
    pub(super) fn new(
        session: Option<Arc<dyn SandboxSession>>,
        gate: Arc<dyn ModeGate>,
        home: Option<&Path>,
    ) -> Self {
        let (undo_stack, snapshot_store) = match &session {
            Some(s) => {
                let workspace = s.workspace_root();
                let store_root = snapshot_store_root_with_home(&workspace, home);
                let store = super::degrade_with_notice(
                    SnapshotStore::new_at(&workspace, store_root),
                    "snapshot store init failed",
                    "undo unavailable; destructive bash commands will require explicit approval",
                )
                .map(Arc::new);
                // Re-link the undo stack to surviving on-disk snapshots so a
                // resumed session can undo a destructive op from the prior
                // process: the in-memory stack is lost on restart, but the
                // snap-N dirs persist. An empty stack (store init failed, or
                // no surviving snapshots) leaves a restore with nothing to do.
                let stack = Arc::new(Mutex::new(match &store {
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
            activator: None,
            active_skill: None,
            transaction: None,
            background: None,
        }
    }

    /// Connect the host that finishes a shell command the foreground gives up,
    /// and the budget the foreground keeps before passing it over. A zero
    /// budget disables the background move, so every command finishes in place.
    pub(super) fn with_background_commands(
        mut self,
        launcher: Arc<dyn BackgroundCommandLauncher>,
        foreground_budget: Duration,
    ) -> Self {
        self.background = Some((launcher, foreground_budget));
        self
    }

    /// Wire the write journal into the file-touch tools, so every file they
    /// write keeps what the target held before the write.
    pub(super) fn with_transaction(mut self, transaction: Option<Arc<FileTransaction>>) -> Self {
        self.transaction = transaction;
        self
    }

    /// Wire the paths-skill activator into the file-touch tools so a touch
    /// activates matching conditional skills.
    pub(super) fn with_activator(
        mut self,
        activator: Option<Arc<dyn ConditionalSkillActivator>>,
    ) -> Self {
        self.activator = activator;
        self
    }

    /// Wire the skill attribution shared by the runner and skill tool.
    pub(super) fn with_active_skill(mut self, cell: Option<Arc<Mutex<Option<String>>>>) -> Self {
        self.active_skill = cell;
        self
    }

    /// The shared undo stack (cloned into the BashTool and set on the Runner,
    /// which restores from it). None when no session or the snapshot store
    /// failed.
    pub(super) fn undo_handles(&self) -> Option<(Arc<Mutex<UndoStack>>, Arc<SnapshotStore>)> {
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
            let mut bash = bash.with_active_skill(self.active_skill.clone());
            if let Some((launcher, budget)) = &self.background {
                bash = bash.with_background_commands(Arc::clone(launcher), *budget);
            }
            v.push(Arc::new(GuardedTool::new(
                Arc::new(bash),
                self.gate.clone(),
            )));
            v.push(Arc::new(GuardedTool::new(
                Arc::new(ReadTool::new(session.clone()).with_activator(self.activator.clone())),
                self.gate.clone(),
            )));
            let write = WriteTool::new(session.clone())
                .with_activator(self.activator.clone())
                .with_transaction(self.transaction.clone());
            v.push(Arc::new(GuardedTool::new(
                Arc::new(write),
                self.gate.clone(),
            )));
            let edit = EditTool::new(session.clone())
                .with_activator(self.activator.clone())
                .with_transaction(self.transaction.clone());
            v.push(Arc::new(GuardedTool::new(
                Arc::new(edit),
                self.gate.clone(),
            )));
            let multiedit = MultiEditTool::new(session.clone())
                .with_activator(self.activator.clone())
                .with_transaction(self.transaction.clone());
            v.push(Arc::new(GuardedTool::new(
                Arc::new(multiedit),
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
