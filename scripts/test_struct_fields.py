#!/usr/bin/env python3
"""Regression tests for check_struct_fields (owner-registry driven).

Per-owner strict-pin: exact == green, growth == red, drift == red, and a
registered owner that disappears == red (renaming cannot evade the gate).
The global raw total is report-only -- never an argument to the gate.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_struct_fields import evaluate_owner, evaluate_registry  # noqa: E402
from rules.monitored_structs import ACTIVE_OWNERS  # noqa: E402

REGISTRY = {"crates/houyicoder-tui/src/state.rs:App": {"fields": 173}}


def test_owner_growth_blocks():
    assert evaluate_owner(174, 173) == 1


def test_owner_drift_blocks():
    assert evaluate_owner(172, 173) == 1


def test_owner_exact_green():
    assert evaluate_owner(173, 173) == 0


def test_registry_green():
    counts = {k: v["fields"] for k, v in REGISTRY.items()}
    counts["crates/houyicoder-tui/src/state.rs:Runner"] = 50
    assert evaluate_registry(counts, REGISTRY) == []


def test_registry_growth_blocks():
    counts = {k: v["fields"] + 1 for k, v in REGISTRY.items()}
    errors = evaluate_registry(counts, REGISTRY)
    assert len(errors) == 1 and "grew" in errors[0]


def test_registry_drift_blocks():
    counts = {k: v["fields"] - 1 for k, v in REGISTRY.items()}
    errors = evaluate_registry(counts, REGISTRY)
    assert len(errors) == 1 and "dropped" in errors[0]


def test_registry_rename_blocks():
    # The owner renamed to AppModel without re-registering: the old key
    # disappears -> red. Renaming cannot present itself as progress.
    counts = {"crates/houyicoder-tui/src/state.rs:AppModel": 173}
    errors = evaluate_registry(counts, REGISTRY)
    assert len(errors) == 1 and "disappeared" in errors[0]


def test_registry_owner_deleted_blocks():
    errors = evaluate_registry({}, REGISTRY)
    assert len(errors) == 1 and "disappeared" in errors[0]


def test_registry_covers_real_owners():
    # Every registered owner must point at a real source file (the key is
    # "path:TypeName"), so the registry cannot drift into monitoring
    # nothing.
    for key in ACTIVE_OWNERS:
        file_path = key.rsplit(":", 1)[0]
        assert (Path(__file__).resolve().parents[1] / file_path).exists(), key


def test_global_total_not_gated():
    # A wildly different global total with the owner exact stays green:
    # wrapper growth is expected during the migration (report-only).
    counts = {k: v["fields"] for k, v in REGISTRY.items()}
    counts["crates/houyicoder-tui/src/state.rs:Runner"] = 5000
    assert evaluate_registry(counts, REGISTRY) == []


if __name__ == "__main__":
    from test_runner import run
    sys.exit(run("struct-fields", dict(globals())))
