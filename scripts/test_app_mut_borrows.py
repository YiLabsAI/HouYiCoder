#!/usr/bin/env python3
"""Regression test for check_app_mut_borrows: locks the strict-pin behavior.

Both directions block (growth AND drift), not just growth. Drift blocking
prevents the silent-baseline-stale failure. Three cases pin: exact ==
green, growth +1 == red, drift -1 == red.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_app_mut_borrows import evaluate, MUT_APP_BASELINE  # noqa: E402


def test_growth_blocks():
    # total > baseline -> 1 (broad-access sigs must not proliferate)
    assert evaluate(MUT_APP_BASELINE + 1) == 1


def test_drift_blocks():
    # strict-pin: total < baseline -> 1 too, not advisory. The floor
    # tracks reality; drift is red until the baseline is lowered.
    assert evaluate(MUT_APP_BASELINE - 1) == 1


def test_exact_green():
    # total == baseline -> 0 (the only green state)
    assert evaluate(MUT_APP_BASELINE) == 0


if __name__ == "__main__":
    g = {k: v for k, v in globals().items() if k.startswith("test_")}
    for name, fn in g.items():
        fn()
        print(f"  ok  {name}")
    print(f"=== {len(g)} passed ===")
