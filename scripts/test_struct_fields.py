#!/usr/bin/env python3
"""Regression test for check_struct_fields: locks the strict-pin behavior.

The value of this gate is that BOTH directions block (growth AND drift),
not just growth. Drift blocking is what prevents the silent-baseline-stale
failure (a count that dropped without the baseline being lowered). These
three cases pin: exact == green, growth +1 == red, drift -1 == red.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_struct_fields import evaluate, STRUCT_FIELD_BASELINE  # noqa: E402


def test_growth_blocks():
    # total > baseline -> 1 (a God-struct refactor must not add fields)
    assert evaluate(STRUCT_FIELD_BASELINE + 1) == 1


def test_drift_blocks():
    # strict-pin: total < baseline -> 1 too, not advisory. The floor
    # tracks reality; drift is red until the baseline is lowered.
    assert evaluate(STRUCT_FIELD_BASELINE - 1) == 1


def test_exact_green():
    # total == baseline -> 0 (the only green state)
    assert evaluate(STRUCT_FIELD_BASELINE) == 0


if __name__ == "__main__":
    g = {k: v for k, v in globals().items() if k.startswith("test_")}
    for name, fn in g.items():
        fn()
        print(f"  ok  {name}")
    print(f"=== {len(g)} passed ===")
