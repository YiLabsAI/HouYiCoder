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
        # picker consolidates six model fields into one.
        "fields": 165,
        "mut_app": 44,
    },
}
