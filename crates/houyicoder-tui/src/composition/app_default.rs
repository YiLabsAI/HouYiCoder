//! Default application state used before runtime services are attached.

use super::*;

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use houyicoder_protocol::frontend::SessionId;

use crate::display_rows_cache::DisplayRowsCache;
use crate::history::HistoryNav;
use crate::hooks_state::HooksPaneState;
use crate::input::InputField;
use crate::last_row_keys::LastRowKeys;
use crate::layout_feedback::LayoutFeedback;
use crate::layout_snapshot::LayoutSnapshot;
use crate::list_pane_state::ListPaneState;
use crate::memory_state::MemoryPaneState;
use crate::notifications::NotificationState;
use crate::paste::{PasteStore, PendingPaste};
use crate::permission_pane_state::PermissionPaneState;
use crate::render_cache::RenderCache;
use crate::resume_picker::SessionPickerState;
use crate::run_metrics::RunMetrics;
use crate::run_state::RunState;
use crate::selection::SystemClipboard;
use crate::selections::Selections;
use crate::session::capabilities::SessionCapabilities;
use crate::session::persistence::SessionPersistence;
use crate::skill_picker_state::SkillPickerState;
use crate::skills_state::SkillsPaneState;
use crate::state::history_read::HistoryReads;
use crate::state::transcript::Transcript;
use crate::state::{
    ModelPickerState, ParkedViewStates, QueueViewState, StatusTab, TrajectoryPaneState,
};

pub fn app() -> App {
    App {
        screen: Screen::Login,
        stage: Stage::Idle,
        pane: Pane::Transcript,
        viewport: ViewportMode::Working,
        prev_viewport: ViewportMode::Working,
        input: InputField::new(),
        history: HistoryNav::default(),
        transcript: Transcript::default(),
        history_reads: HistoryReads::default(),
        verdict_cursor: 0,
        transcript_view: Default::default(),
        display_rows_cache: {
            let c = DisplayRowsCache::default();
            c.version.set(u64::MAX);
            c
        },
        indexing: Cell::new(false),
        indexed_bytes: Cell::new(0),
        index_total: Cell::new(0),
        index_done: Cell::new(false),
        pending_index_chunk: std::cell::RefCell::new(None),
        persistence: SessionPersistence::default(),
        resume_picker: SessionPickerState::default(),
        pending_resume_target: None,
        palette: PaletteState::default(),
        prompt: None,
        status: status(),
        spec_ctx: spec_context(),
        spec_clauses: spec_clauses(),
        diff: diff_data(),
        spec_artifact: spec_artifact(),
        plan_artifact: plan_artifact(),
        review: ReviewQueue {
            findings: review_findings(),
            focus: 0,
            audit_trail: audit_trail(),
        },
        console: ConsoleState {
            todos: console_todos(),
        },
        verify_result: verify_result(),
        graph_result: graph_result(),
        memory: MemoryPaneState::new(memory_entries()),
        queue_view: QueueViewState::default(),
        worktree_entries: Vec::new(),
        worktree_list: ListPaneState::default(),
        worktree_level: Cell::new(0),
        trajectory: TrajectoryPaneState::default(),
        agents: Default::default(),
        agent_directory: None,
        artifact: ArtifactSession::stub(),
        proposer: StubProposer::new(),
        login_mode: None,
        stage_history: Vec::new(),
        replaying: false,
        quit: false,
        session_id: SessionId::new("test"),
        runtime: None,
        session: None,
        notifications: NotificationState::default(),
        terminal_focused: true,
        run_state: RunState::Idle,
        run_metrics: RunMetrics::default(),
        todos: Default::default(),
        layout_feedback: LayoutFeedback::default(),
        displayed_tokens: Cell::new(0),
        last_run_input: None,
        pending: Vec::new(),
        selections: Selections::default(),
        layout_snapshot: LayoutSnapshot::default(),
        render_cache: RefCell::new(RenderCache::default()),
        last_row_keys: LastRowKeys::default(),
        parked_view_states: ParkedViewStates::default(),
        teammate_view: None,
        fleet: Default::default(),
        verbose: false,
        pasted: PasteStore::default(),
        pending_paste: PendingPaste::default(),
        capabilities: SessionCapabilities::default(),
        permission_pane: PermissionPaneState::default(),
        working_dir: cwd_string(),
        skills_pane: SkillsPaneState::default(),
        skill_picker: SkillPickerState::default(),
        hooks_pane: HooksPaneState::default(),
        loaded_from_frame: Cell::new(usize::MAX),
        model_picker: ModelPickerState::default(),
        status_tab: StatusTab::default(),
        status_name_edit: None,
        last_title: None,
        clipboard: Arc::new(SystemClipboard),
    }
}
