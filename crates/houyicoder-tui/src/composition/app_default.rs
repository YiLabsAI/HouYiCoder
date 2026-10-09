//! Default application state used before runtime services are attached.

use super::*;

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use houyicoder_protocol::frontend::SessionId;

use crate::display_rows_cache::DisplayRowsCache;
use crate::hooks_state::HooksPaneState;
use crate::input::Input;
use crate::last_row_keys::LastRowKeys;
use crate::layout_feedback::LayoutFeedback;
use crate::layout_snapshot::LayoutSnapshot;
use crate::list_pane_state::ListPaneState;
use crate::memory_state::MemoryPaneState;
use crate::notifications::NotificationState;
use crate::permission_pane_state::PermissionPaneState;
use crate::render_cache::RenderCache;
use crate::resume_picker::SessionPickerState;
use crate::selection::SystemClipboard;
use crate::selections::Selections;
use crate::session::Session;
use crate::session::identity::SessionIdentity;
use crate::session::persistence::SessionPersistence;
use crate::skill_picker_state::SkillPickerState;
use crate::skills_state::SkillsPaneState;
use crate::state::{
    ModelPickerState, ParkedViewStates, QueueViewState, StatusTab, TrajectoryPaneState,
};
use crate::workflow::Workflow;

pub fn app() -> App {
    App {
        screen: Screen::Login,
        workflow: workflow(),
        pane: Pane::Transcript,
        viewport: ViewportMode::Working,
        prev_viewport: ViewportMode::Working,
        input: Input::default(),
        verdict_cursor: 0,
        transcript_view: Default::default(),
        display_rows_cache: {
            let c = DisplayRowsCache::default();
            c.version.set(u64::MAX);
            c
        },
        persistence: SessionPersistence::default(),
        resume_picker: SessionPickerState::default(),
        pending_resume_target: None,
        palette: PaletteState::default(),
        status: status(),
        memory: MemoryPaneState::new(memory_entries()),
        queue_view: QueueViewState::default(),
        worktree_entries: Vec::new(),
        worktree_list: ListPaneState::default(),
        worktree_level: Cell::new(0),
        trajectory: TrajectoryPaneState::default(),
        agents: Default::default(),
        agent_directory: None,
        login_mode: None,
        quit: false,
        session: Session::bare(SessionIdentity {
            session_id: SessionId::new("test"),
            working_dir: cwd_string(),
        }),
        notifications: NotificationState::default(),
        terminal_focused: true,
        layout_feedback: LayoutFeedback::default(),
        displayed_tokens: Cell::new(0),
        pending: Vec::new(),
        selections: Selections::default(),
        layout_snapshot: LayoutSnapshot::default(),
        render_cache: RefCell::new(RenderCache::default()),
        last_row_keys: LastRowKeys::default(),
        parked_view_states: ParkedViewStates::default(),
        teammate_view: None,
        fleet: Default::default(),
        verbose: false,
        permission_pane: PermissionPaneState::default(),
        skills_pane: SkillsPaneState::default(),
        skill_picker: SkillPickerState::default(),
        hooks_pane: HooksPaneState::default(),
        model_picker: ModelPickerState::default(),
        status_tab: StatusTab::default(),
        status_name_edit: None,
        last_title: None,
        clipboard: Arc::new(SystemClipboard),
    }
}

/// The seeded workflow state: the idle stage machine plus the stub spec,
/// review, verify, artifact, and inbox defaults the composition starts from.
/// A session switch rebuilds the whole App, so this is the single reset point.
fn workflow() -> Workflow {
    Workflow {
        stage: Stage::Idle,
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
        artifact: ArtifactSession::stub(),
        proposer: StubProposer::new(),
        stage_history: Vec::new(),
        replaying: false,
    }
}
