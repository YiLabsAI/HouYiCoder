#!/usr/bin/env python3
"""Regression test for check_file_size: locks the test/prod split + the
continuous-excess properties.

The value of this gate lives in two things the old single-threshold
gate could not express: (1) test and production files carry different
cognitive cost, so the per-file error limit splits; (2) the aggregate
excess metric is continuous -- no cliff at the 700 threshold. A count
ratchet (count of files >= N) would recreate the cliff it is meant to
replace at N; the continuous sum has no position where a 1-line growth
trips a discontinuity. These four cases lock both.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_file_size import evaluate, ratchet_status, EXCESS_BASELINE  # noqa: E402

PROD = "crates/x/src/foo.rs"
TEST = "crates/x/src/foo_tests.rs"


def _prod(n):
    return evaluate([(PROD, n)])


def _test(n):
    return evaluate([(TEST, n)])


def test_prod_file_over_800_hits_per_file_error():
    # Per-file cliff stays for production: 850 >= 800 -> error.
    r = _prod(850)
    assert r["errs"], "prod 850 must trip the per-file error"
    assert r["excess"] == 150  # 850 - 700


def test_test_file_at_850_does_not_error():
    # The split: a test file at 850 is fine (test error is 2000).
    r = _test(850)
    assert not r["errs"], "test 850 must not trip the per-file error (split)"
    # test files do not contribute to production excess
    assert r["excess"] == 0


def test_699_to_700_changes_excess_by_zero_no_cliff():
    # THE core case: crossing the 700 threshold adds +0 to excess.
    # A count ratchet would +1 here -- recreating the cliff at 700.
    # The continuous metric has no discontinuity at the threshold.
    at_699 = _prod(699)["excess"]
    at_700 = _prod(700)["excess"]
    assert at_699 == 0 and at_700 == 0, "699->700 must change excess by 0"
    assert _prod(701)["excess"] == 1, "701 -> excess 1 (continuous from 0)"


def test_750_to_760_adds_ten_continuous():
    # Continuity in the danger band: 750 -> 760 is +10, not a cliff jump.
    assert _prod(750)["excess"] == 50
    assert _prod(760)["excess"] == 60


def test_split_drops_excess_proportionally():
    # A 798 split into 400 + 398 (both under 700) drops excess by 98 --
    # the metric rewards real refactoring, not just threshold-dodging.
    before = _prod(798)["excess"]
    after = evaluate([(PROD, 400), (PROD, 398)])["excess"]
    assert before == 98 and after == 0


def test_test_file_does_not_swallow_prod_excess():
    # A test file at 1500 contributes 0 to production excess.
    r = evaluate([(PROD, 798), (TEST, 1500)])
    assert r["excess"] == 98


def test_excess_ratchet_growth_blocks():
    # excess > baseline -> 1 (production bloat must not grow)
    assert ratchet_status(EXCESS_BASELINE + 1) == 1


def test_excess_ratchet_improvement_passes():
    # ceiling mode: excess < baseline -> 0 (improvements pass silently)
    assert ratchet_status(EXCESS_BASELINE - 1) == 0


def test_excess_ratchet_at_ceiling_green():
    # excess == baseline -> 0 (at the ceiling, still green)
    assert ratchet_status(EXCESS_BASELINE) == 0


if __name__ == "__main__":
    g = {k: v for k, v in globals().items() if k.startswith("test_")}
    for name, fn in g.items():
        fn()
        print(f"  ok  {name}")
    print(f"=== {len(g)} passed ===")
