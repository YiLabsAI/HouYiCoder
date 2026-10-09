//! The workflow's current state, bundled as one owner toward App's final
//! shell. The workflow model itself (linear stage chain, DAG, or composable
//! steps) is still open; this owner only groups the fields their current
//! behavior shares so a session switch resets them as one value.

use crate::artifact::{ArtifactSession, StubProposer};
use crate::console_state::ConsoleState;
use crate::evidence::{
    DiffData, GraphResult, PlanArtifact, SpecArtifact, SpecClause, VerifyResult,
};
use crate::records::SpecContext;
use crate::review_queue::ReviewQueue;
use crate::state::enums::Stage;

/// The spec-plan-review-verify-artifact lifecycle fields of the frontend.
/// Final workflow semantics are a separate design; the fields travel
/// together here so the App shell carries one owner, and the composition
/// root builds the workflow as a single value.
pub(crate) struct Workflow {
    /// The current stage of the spec-to-signoff flow.
    pub(crate) stage: Stage,
    /// The spec context the stage machine steps through.
    pub(crate) spec_ctx: SpecContext,
    /// The spec clauses produced or approved this session.
    pub(crate) spec_clauses: Vec<SpecClause>,
    /// The diff under review in the implementing stage.
    pub(crate) diff: DiffData,
    /// The spec artifact drafted this session.
    pub(crate) spec_artifact: SpecArtifact,
    /// The plan artifact drafted this session.
    pub(crate) plan_artifact: PlanArtifact,
    /// The review queue of findings awaiting sign-off.
    pub(crate) review: ReviewQueue,
    /// The console work-inbox todo column. Constructed but not yet rendered;
    /// the console pane draws the review queue and connects this inbox later.
    #[allow(dead_code)]
    pub(crate) console: ConsoleState,
    /// The verification result of the current pass.
    pub(crate) verify_result: VerifyResult,
    /// The graph-build result of the current pass.
    pub(crate) graph_result: GraphResult,
    /// The opened workspace artifact for inline review and annotation.
    pub(crate) artifact: ArtifactSession,
    /// The proposer that turns an annotation into a pending edit.
    pub(crate) proposer: StubProposer,
    /// The stack of stages the chain moved through, for rewind.
    pub(crate) stage_history: Vec<Stage>,
    /// True while a canned replay indicator is on screen.
    pub(crate) replaying: bool,
}
