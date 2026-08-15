#!/usr/bin/env python3
"""Regression test for check_module_dead_code: locks the strict-pin behavior.

Both directions block (growth AND drift), not just growth. Three cases
pin: exact == green, growth +1 == red, drift -1 == red.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_module_dead_code import evaluate, MODULE_DEAD_CODE_BASELINE  # noqa: E402


def test_growth_blocks():
    # count > baseline -> 1 (a new whole-file suppression is a blind spot)
    assert evaluate(MODULE_DEAD_CODE_BASELINE + 1) == 1


def test_drift_blocks():
    # strict-pin: count < baseline -> 1 too, not advisory. The floor
    # tracks reality; drift is red until the baseline is lowered.
    assert evaluate(MODULE_DEAD_CODE_BASELINE - 1) == 1


def test_exact_green():
    # count == baseline -> 0 (the only green state)
    assert evaluate(MODULE_DEAD_CODE_BASELINE) == 0


if __name__ == "__main__":
    g = {k: v for k, v in globals().items() if k.startswith("test_")}
    for name, fn in g.items():
        fn()
        print(f"  ok  {name}")
    print(f"=== {len(g)} passed ===")
