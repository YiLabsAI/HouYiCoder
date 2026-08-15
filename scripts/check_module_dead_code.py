#!/usr/bin/env python3
"""Module dead_code ratchet (strict-pin, L2): the count of module-level
#![allow(dead_code)] whole-file suppressions must equal the baseline.

Growth blocks: a new whole-file suppression is a blind spot -- narrow it
to the specific dead item, remove the dead code, or justify the bump.
Drift blocks: a removed suppression must lower the baseline so the floor
tracks reality. Strict-pin: both directions return 1.

The detector + file list live in report_structure_facts (the L3
report-only counterpart in make verify); this is the blocking version
in make check.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from report_structure_facts import (  # noqa: E402
    MODULE_DEAD_CODE_BASELINE,
    module_dead_code_ratchet,
)


def evaluate(count, baseline=MODULE_DEAD_CODE_BASELINE) -> int:
    """Pure strict-pin: 0 only when count == baseline. Growth (>) and
    drift (<) both return 1. Pure so the regression test can lock both
    directions without scanning files."""
    return 0 if count == baseline else 1


def main() -> int:
    r = module_dead_code_ratchet()
    count = r["current"]
    if count > MODULE_DEAD_CODE_BASELINE:
        print(
            f"error: module dead_code ratchet breached: {count} > "
            f"{MODULE_DEAD_CODE_BASELINE} (+{count - MODULE_DEAD_CODE_BASELINE}). "
            f"A new whole-file #![allow(dead_code)] is a blind spot -- narrow "
            f"the suppression to the specific dead item, or remove the dead "
            f"code. If the whole-file suppression is genuinely needed, bump "
            f"MODULE_DEAD_CODE_BASELINE with a reason.",
            file=sys.stderr,
        )
        for f in r["files"][:8]:
            print(f"  {f}", file=sys.stderr)
        return 1
    if count < MODULE_DEAD_CODE_BASELINE:
        print(
            f"error: module dead_code ratchet drifted: {count} < "
            f"{MODULE_DEAD_CODE_BASELINE} (-{MODULE_DEAD_CODE_BASELINE - count}). "
            f"Lower MODULE_DEAD_CODE_BASELINE to {count} in this commit so "
            f"the floor tracks reality (strict-pin: drift blocks until fixed).",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
