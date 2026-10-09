#!/usr/bin/env python3
"""Structs monitored by the structural gates, with their budgets.

The active registry lists the central types monitored by the field-count
and broad-access gates. Each entry is registered atomically with the
commit that introduces the owner: register the fully-qualified path, set
the baseline, and extend the negative tests. A registered type that
disappears is a gate failure; a future owner that does not exist yet is
not. Per-owner baselines are strict-pin. The global raw field total is
report-only during the migration -- splitting fields into wrappers
legitimately grows it, so growth is printed as a trend, never silently
green, and never blocking.
"""
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

ACTIVE_OWNERS = {
    "crates/houyicoder-tui/src/state.rs:App": {
        # RunState owns the run lifecycle (four fields removed); the model
        # picker consolidates six model fields into one. parked_view_states
        # carries the four expansion sets across a session switch. The turn
        # counter went with the live turn summary it named. Transcript took
        # the turn boundary and the render version (two fields; the lines
        # field changed type in place). verdict_cursor stays as an audit
        # capability; scrolled_from_frame waits for the view-state owner.
        # RunProgress took the eight streaming-progress fields (assistant
        # preview, reasoning preview, live block, the two timing clocks,
        # the running tool set, the bash ticker map) into ActiveRun, so
        # they live with the run and drop on finish instead of lingering.
        # Field access is routed through run_progress_mut() on App, so no
        # function signature changed and the mut_app count holds.
        # PendingPrompt absorbed the seven reverse-request fields (approval
        # card, interactive question card, permission request id, approval
        # batch, trust prompt, trust choice, trust request id) into one
        # prompt field that holds at most one live ask, net six fewer.
        # Transcript took the ordered frame log (one field). The trajectory
        # pane's five parallel fields (drill level, cursor, body length, the
        # drilled row, the background-row flag) became one TrajectoryPaneState
        # owner, and the selected turn lives inside it rather than joining the
        # shell: the selection and the cursor that must agree are kept
        # consistent by the same transitions. The skills pane's level and
        # selection moved into SkillsPaneState in the same spirit. The index
        # build's pending-chunk slot joins the index fields it belongs to; the
        # window's fields are a SearchWindowState owner waiting to be drawn.
        # HistoryReads took the pending-read slot out of Transcript and App
        # gained the owner field beside it, net one more. The transcript
        # view state took the two scroll objects and the window flag
        # (three flat fields gone, one owner field gained, net two fewer):
        # the whole-vec scroll and the search window's scroll are two
        # instances of the one scroll type, and which one is live is a
        # method on the owner rather than a bare boolean every caller
        # re-reads. The search corpus (frozen lines plus load metadata)
        # joined the view state next, taking four flat fields: enter and
        # exit are transactions rather than assignments a caller repeats.
        # The byte-window (history window) joined beside it, taking the
        # anchor, end, frozen-size, and per-window skip fields: a window
        # navigation step is now one replace call, not three assignments.
        # The four expansion sets (results, fold groups, thinking,
        # subagents) moved into the view state owner beside them, so a
        # view park or restore swaps one value, not four fields a caller
        # moves by hand. The search query state followed: it lives beside
        # the corpus so the run path holds the corpus slice while it
        # mutates the query, a disjoint field borrow a method cannot.
        # The unseen-since cursor joined beside it: the scroll-away
        # snapshot the new-message count reads against the tail. The four
        # per-row key vectors of the last emitted visible row set (call id,
        # fold-group key, expanded-group mark, turn id) became one LastRowKeys
        # owner beside render_cache, so the draw pass writes and the click and
        # Ctrl+O handlers read one set that cannot drift out of length sync.
        # The pre-rendered display-row cache (the rows, the version stamp that
        # guards them, the four per-row key vectors, and the pre-rendered
        # lines) became one DisplayRowsCache owner: the six are recomputed in
        # lockstep on a version mismatch and sliced in lockstep every frame,
        # so one owner keeps a stale-length vector from desyncing a key.
        # The permission pane state (the selected tab, the cursor row, the
        # typed-input sub-mode, and the pane-local search buffer) became one
        # PermissionPaneState owner: a tab switch or input-mode entry replaces
        # the whole value and resets the cursor and buffer, which the flat
        # fields could only carry as a comment.
        # The hooks pane drill level and selection cursor became one
        # HooksPaneState owner, so enter and return step the level and clamp
        # the cursor as one value.
        # The pending paste chunks and the last-chunk timestamp became one
        # PendingPaste owner, so a flush check drains and restores both as
        # one value rather than two flat fields a caller must move in step.
        # The renderer layout feedback (terminal height, transcript pane
        # width, input wrap column count, and the native cursor position the
        # input method parked) became one LayoutFeedback owner: all four are
        # written by the draw pass and read by the count, layout, and input
        # paths, so they travel as the render's measured feedback rather than
        # four loose cells.
        # The @ skill-picker overlay's open flag and list cursor became one
        # SkillPickerState owner, so opening the picker resets the cursor to
        # zero as one value rather than two flat fields a caller sets in step.
        # The renderer layout snapshot gathered the ten render-diff fields (five
        # screen rects and five last-rendered-row caches: transcript, pane,
        # status, approval regions plus the transcript full-rows cache and the
        # jump-to-bottom label rect) into one LayoutSnapshot owner, so each
        # region's rect and its rows are written together after the draw and
        # read together by the mouse-hit and selection paths.
        # The four per-region drag selections (transcript, status, pane,
        # approval) became one Selections owner, so each region's selection
        # travels beside its siblings rather than as four loose App fields.
        # The agent sender went to the connection/harness assembly, not an App
        # field; nothing on App read it.
        # The permission state (mode, rules, directories, git toggle, sticky
        # verdicts, verdict log) became one PermissionCapability under the
        # session's SessionCapabilities composition; six flat fields folded
        # into the one App field named capabilities.
        # The status caches (context breakdown, status snapshot, the last
        # poll stamp, and the pending-command flag) became one StatusCapability
        # under the same composition; four flat fields folded into the one App
        # field named capabilities.
        # The entry caches (registered hooks, registered tools, discovered
        # skills, and the session disable set) became three capabilities under
        # the same composition; four flat fields folded into the one App field
        # named capabilities.
        # The four read-side persistence collaborators (trajectory source,
        # export writer, transcript snapshot, session catalog) became one
        # SessionPersistence owner; the composition root injects them together,
        # so the panes and commands read one owner rather than four fields.
        # The session's cumulative run metrics (first-run start, output token
        # tally, model-call steps) became one RunMetrics owner, so the
        # completion path records one value and a session switch resets it
        # whole.
        "fields": 80,
        "mut_app": 44,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:PermissionCapability": {
        # The session's wire-reported permission state: mode, rules, allowed
        # directories, the ask-before-git toggle, per-tool sticky verdicts,
        # and the verdict audit trail. Six cache fields refreshed by the same
        # wire replies and read by the same pane and status paths, so they
        # move as one capability value.
        "fields": 6,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:StatusCapability": {
        # The session's status caches and poll bookkeeping: the last context
        # breakdown, the latest status snapshot, the last poll stamp, and the
        # pending-command flag. Four flat fields folded into one capability
        # value under the session's capability composition.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:SessionCapabilities": {
        # The static composition of a session's named capabilities: the
        # permission, status, hooks, tools, and skills capabilities. The
        # connection, run, and transcript live beside it, not inside it.
        "fields": 5,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:HooksCapability": {
        # The session's registered-hook cache: the rows the /hooks pane
        # renders, refreshed by the server reply and read by the pane. One
        # capability value rather than a flat App field beside unrelated state.
        "fields": 1,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:ToolsCapability": {
        # The session's discovered-tool cache: the rows the /tools pane
        # renders, refreshed by the server reply and read by the pane.
        "fields": 1,
    },
    "crates/houyicoder-tui/src/session/capabilities.rs:SkillsCapability": {
        # The session's discovered-skill cache and the session disable set:
        # the rows the /skills pane renders plus the names the user disabled.
        # One capability value so the pane reads the entries and the disables
        # from one owner rather than two flat App fields.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/session/persistence.rs:SessionPersistence": {
        # The session's injected persistence collaborators: the trajectory
        # reader, the transcript snapshot reader, the export writer, and the
        # resumable-session catalog. Four flat App fields folded into one
        # owner, injected together by the composition root.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/run_metrics.rs:RunMetrics": {
        # The session's cumulative run metrics: the first-run start and the
        # output tokens and model-call steps accrued across completed runs.
        # Three flat App fields folded into one owner, reset on session switch.
        "fields": 3,
    },
    "crates/houyicoder-tui/src/last_row_keys.rs:LastRowKeys": {
        # The four per-row key vectors of the last emitted visible row set:
        # the call id, fold-group key, expanded-group mark, and turn id each
        # visible row carries, parallel arrays the click handler and the draw
        # pass read by visible-row index. One owner so the four are written and
        # read as one set, never drifted out of sync.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/display_rows_cache.rs:DisplayRowsCache": {
        # The pre-rendered display-row set the draw pass caches by version:
        # the rows, the version stamp, the four per-row key vectors, and the
        # pre-rendered lines. One owner because the six are recomputed in
        # lockstep when the version is stale and sliced in lockstep every
        # frame, so no vector drifts to a length the others do not share.
        "fields": 7,
    },
    "crates/houyicoder-tui/src/permission_pane_state.rs:PermissionPaneState": {
        # The permission pane state: the selected tab (the rule filter or the
        # recent verdict log), the cursor row inside it, the typed-input
        # sub-mode (add / remove / search / none), and the pane-local search
        # buffer. One owner because a tab switch resets the cursor and an
        # input-mode entry resets the cursor and buffer, so the four move as
        # one value rather than four fields a caller must reset in step.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/hooks_state.rs:HooksPaneState": {
        # The hooks pane browse state: the drill level (0 = event list, 1 =
        # the selected event detail) and the selection cursor in the
        # level-0 list. One owner because enter and return step the level and
        # clamp the cursor together, never one without the other.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/skill_picker_state.rs:SkillPickerState": {
        # The @ skill-picker overlay state: the open flag and the list cursor.
        # One owner because opening the picker resets the cursor to zero and
        # selecting or closing clears it, so the two move as one value rather
        # than two flat fields a caller must reset in step.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/paste.rs:PendingPaste": {
        # The bracketed-paste accumulator: the chunk buffer built up across
        # Paste events and the last-chunk timestamp, drained and restored
        # together on each 50ms gap flush check. One owner because a flush
        # that drained the buffer but left the timestamp (or vice versa) left
        # the next check with a timestamp and no buffer to flush.
        "fields": 2,
    },
    "crates/houyicoder-tui/src/layout_feedback.rs:LayoutFeedback": {
        # The renderer layout feedback: the terminal height, transcript pane
        # width, input wrap column count, and the native cursor position the
        # input method parked, all Cell mirrors the draw pass writes and the
        # count, layout, and input paths read. One owner because together
        # they hold the dims and caret of the last render, so a count path
        # that read one dim while the draw pass updated another saw a
        # render/count mismatch.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/layout_snapshot.rs:LayoutSnapshot": {
        # The renderer layout snapshot: per-region rectangles and rendered-row
        # caches the draw pass stashes, read back by the input and click paths.
        # One owner because each region's rect and its rows are written
        # together after the draw and read together by the hit-test paths.
        "fields": 10,
    },
    "crates/houyicoder-tui/src/selections.rs:Selections": {
        # The four per-region drag selections: the transcript's main selection
        # plus the status, pane, and approval selections, each a Selection in
        # its own coordinate space. One owner so a drag on one region never
        # collides with another and the four travel as one value rather than
        # as four loose App fields.
        "fields": 4,
    },
    "crates/houyicoder-tui/src/state/transcript/view.rs:TranscriptViewState": {
        # The transcript view's own state, one value per view: the whole-vec
        # scroll, the search window's scroll, the window flag, the search
        # corpus, the byte-window (history window), the four expansion sets
        # bundled as one value, the search query state, and the unseen-since
        # cursor. The dispatch (which scroll is live) is a method here, so
        # callers cannot re-derive it from the flag.
        "fields": 8,
    },
    "crates/houyicoder-tui/src/state/transcript.rs:Transcript": {
        # The transcript facts: frame log, resident budget counters, derived
        # lines and fold cache, disk rows and their front, turn boundary,
        # revision. The pending-read slot moved out to HistoryReads: the
        # transcript owns facts, not task lifecycles. External mutators are
        # the rebuild path and the command layer, both through pub(crate)
        # methods; the frame log mutates only through with_frames_mut so the
        # byte counter cannot miss an edit.
        "fields": 12,
    },
    "crates/houyicoder-tui/src/state/history_read.rs:HistoryReads": {
        # The record on file, the workers holding a slot, and the generation
        # counter. External mutators: dispatch/take/put_back from the rebuild
        # path, invalidate from the command layer on a history reset. The epoch
        # is the guard that outlives the record: one held across an invalidation
        # is still refused by its stamp. The worker count is shared with every
        # permit handed out, so the budget reads what is executing rather than
        # what the record still names.
        "fields": 3,
    },
    "crates/houyicoder-tui/src/state/parked_view_states.rs:ParkedViewStates": {
        # Expansion sets parked for views that are not on screen: by session,
        # by child within the current visit, and the parent's own sets held
        # while a child view is open. Both lists are capped, and a repeat park
        # of the same view replaces its earlier entry, so the entry count
        # follows the views visited rather than the visit count. The
        # transitions exchange whole sets, which is what keeps one view's keys
        # out of another.
        "fields": 3,
    },
}
