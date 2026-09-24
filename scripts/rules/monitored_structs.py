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
        # picker consolidates six model fields into one. parked_keys carries
        # the four expansion sets across a session switch. The turn counter
        # went with the live turn summary it named. Transcript took the turn
        # boundary and the render version (two fields; the lines field
        # changed type in place). verdict_cursor stays as an audit
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
        # consistent by the same transitions.
        "fields": 144,
        "mut_app": 44,
    },
}
