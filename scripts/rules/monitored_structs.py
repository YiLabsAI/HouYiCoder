#!/usr/bin/env python3
"""Structs monitored by the structural gates, with their budgets.

The active registry lists the central types monitored by the field-count and
broad-access gates. Each entry is registered atomically with the commit that
introduces the owner: register the fully-qualified path, set the baseline, and
extend the negative tests. A registered type that disappears is a gate failure;
a future owner that does not exist yet is not. Per-owner baselines are
strict-pin: a change to one is a deliberate decision with a reason beside it.
The global raw field total is report-only during the migration -- splitting
fields into wrappers legitimately grows it, so growth is printed as a trend,
never silently green, and never blocking.

Each comment states the invariant the budget protects, not the history of how
the type reached its present shape.
"""
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

ACTIVE_OWNERS = {
    "crates/houyicoder-tui/src/app/state.rs:App": {
        # The shell: the Ui, the sessions aggregate, and the workflow. The budget guards against a new field landing here instead of on the owner that changes with it.
        "fields": 3,
        "mut_app": 44,
    },
    "crates/houyicoder-tui/src/ui.rs:Ui": {
        # The frontend's presentation and interaction state, including the draw pass's quit flag, renderer, and prepared frame.
        "fields": 25,
    },
    "crates/houyicoder-tui/src/ui/renderer.rs:Renderer": {
        # The draw pass's reusable caches: the per-line parse cache and the
        # transcript's pre-rendered slot rows. Both outlive a frame and are
        # cleared together on a session switch, so nothing parsed or
        # pre-rendered for the old session rides into the new one.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/ui/renderer.rs:PreparedFrame": {
        # The last successfully drawn frame: the measured region rectangles,
        # the feedback dimensions the count and layout paths read back, and
        # the per-row keys the click and Ctrl+O paths resolve against. The
        # draw writes into a staged frame and this is replaced only when the
        # draw reports success, so a reader never acts on a half-written one.
        "fields": 3,
    },
    "crates/houyicoder-tui/src/ui/navigation.rs:Navigation": {
        # Where the user is looking: screen, pane, viewport modes, login mode, status sub-tab, name buffer, and the overlays.
        "fields": 10,
    },
    "crates/houyicoder-tui/src/input.rs:Input": {
        # The composed chat text and the context that edits it: editor, history, both paste halves, and the queued-input FIFO.
        "fields": 5,
    },
    "crates/houyicoder-tui/src/input/queue.rs:InputQueue": {
        # The queued-input FIFO and its selection cursor; the items stay private so the order and the cursor cannot drift apart.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/session.rs:Session": {
        # The presented session: identity, connection, run lifecycle, tally, pending ask, restore stash, checklist, capabilities, transcript read state, and transcript.
        "fields": 16,
    },
    "crates/houyicoder-tui/src/session/sessions.rs:Sessions": {
        # The presented session, the resumable catalog, and the current session's persistence ports, swapped together.
        "fields": 3,
    },
    "crates/houyicoder-tui/src/workflow.rs:Workflow": {
        # The spec-plan-review-verify-artifact lifecycle with its artifacts, rewind stack, and replay flag.
        "fields": 14,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:PermissionCapability": {
        # The server-reported permission caches plus the verdict audit trail and the read position that guards it.
        "fields": 7,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:StatusCapability": {
        # The status caches and poll bookkeeping the status paths read.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:SessionCapabilities": {
        # The static composition of a session's named capabilities; the connection, run, and transcript sit beside it, not inside it.
        "fields": 9,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:HooksCapability": {
        # The registered-hook cache the hooks pane renders.
        "fields": 1,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:ToolsCapability": {
        # The discovered-tool cache the tools pane renders.
        "fields": 1,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:SkillsCapability": {
        # The discovered-skill cache and the names the user disabled.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:WorktreeCapability": {
        # The linked-worktree listing the worktree pane renders.
        "fields": 1,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:FleetCapability": {
        # The child snapshots the host pushes, in first-observed order.
        "fields": 1,
    },
    "crates/houyicoder-tui/src/view/fleet/strip_state.rs:FleetStripState": {
        # The footer fleet strip's selection and the repaint pacing of its live
        # elapsed counters. The geometry one draw produced lives in that
        # frame's layout snapshot, not here.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:MemoryCapability": {
        # The memory entry cache, the two toggle states, the notice dedupe set, and the pending mutations.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/session/capabilities/model.rs:ModelCapability": {
        # The model catalog with the applied model, and the commit awaiting its reply.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/view/model_pane/draft.rs:ModelDraft": {
        # The model pane's draft: focused row, adjusted level and tier, the setting the arrows take, and the dirty flag.
        "fields": 6,
    },
    "crates/houyicoder-tui/src/session/identity.rs:SessionIdentity": {
        # The wire routing id and the working directory, which reset together on a session switch.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/session/persistence.rs:SessionPersistence": {
        # The injected trajectory, snapshot, and export read ports.
        "fields": 3,
    },
    "crates/houyicoder-tui/src/session/run_metrics.rs:RunMetrics": {
        # The cumulative run metrics, reset whole on a session switch.
        "fields": 3,
    },
    "crates/houyicoder-tui/src/session/run_state.rs:ToolRuntime": {
        # The per-call tool runtime: which calls are in flight and the bash
        # ticker with the revision the slot cache keys on. One owner because
        # the ticker and its revision have to move together, and a tick only
        # counts for a call the run still tracks.
        "fields": 3,
    },
    "crates/houyicoder-tui/src/transcript/scan_state.rs:ScanState": {
        # The full scan's progress mirror and the chunk a worker still reads.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/ui/rows/last_row_keys.rs:LastRowKeys": {
        # The per-row keys of the last visible window, all taken from one row set in a single replace.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/ui/rows/display_rows_cache.rs:DisplayRowsCache": {
        # The cached slot rows and the key they were built for.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/ui/rows/row_set.rs:RowSet": {
        # The rows of one view with the metadata each row carries, the identity
        # of the line it belongs to included, plus the line the rows pushed
        # from here on belong to; push and the batch replace are the only
        # writers. The identity is here because a parked viewport names the row
        # it stands on, and the text beside that row is what resolves the offset
        # into it.
        "fields": 9,
    },
    "crates/houyicoder-tui/src/view/capability/state.rs:PermissionPaneState": {
        # The permission pane's tab, cursor, input sub-mode, and search buffer.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/view/hooks_pane/state.rs:HooksPaneState": {
        # The hooks pane's drill level and selection cursor.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/view/skill_picker/state.rs:SkillPickerState": {
        # The skill picker's open flag and list cursor.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/input/paste.rs:PendingPaste": {
        # The bracketed-paste accumulator and the stamp of its last chunk.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/ui/layout_feedback.rs:LayoutFeedback": {
        # The last render's measured dims and the caret position the input method parked.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/ui/layout_snapshot.rs:LayoutSnapshot": {
        # The geometry one draw produced: each content region's rectangle
        # beside the rows it rendered, the jump-to-bottom label, the two
        # footer strips' click rects, and the height the footer budget
        # granted the fleet strip. The byte window's rows sit here too,
        # with the range and width they were built from, because that path
        # rebuilds them every frame and keeps no cache of its own. The input
        # and click paths read the rects and rows; the status-bar hint reads
        # the granted height.
        "fields": 15,
    },
    "crates/houyicoder-tui/src/ui/selections.rs:Selections": {
        # The four per-region drag selections, each in its own coordinate space.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/transcript/view.rs:TranscriptViewState": {
        # One view's scroll, search corpus and query, byte-window, expansion sets, unseen cursor, and presentation revision.
        "fields": 9,
    },
    "crates/houyicoder-tui/src/transcript/store.rs:Transcript": {
        # The transcript facts: frame log, budget counters, derived lines and
        # fold cache, disk rows, turn boundary, the count of rows the frontend
        # raised with no frame behind them, and revision. The raise count is
        # here because it names those rows: their names must stay past every
        # frame position for the life of the list, and a name reused across
        # rebuilds would answer an anchor with a row it does not mean. The
        # front checkpoint is here because it is the turn state of the oldest
        # resident frame, which only the drain advances: held apart from the
        # frame log, a front that moved without its turn state would summarize
        # the wrong turn and orphan a result whose call had left.
        "fields": 14,
    },
    "crates/houyicoder-tui/src/transcript/history_state.rs:HistoryReads": {
        # The pending-read record, the workers holding a slot, the generation that invalidates them, and the retry deadline a failed read sets.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/transcript/parked_view_states.rs:ParkedViewStates": {
        # Expansion sets parked for the views that are not on screen.
        "fields": 3,
    },

}
