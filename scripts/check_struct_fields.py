#!/usr/bin/env python3
"""Struct field-count ratchet (strict-pin): the total field count of
structs with more than FIELD_WARN_THRESHOLD fields must equal the
baseline exactly.

Growth (total > baseline) blocks: a God-struct refactor must move
fields, not add. Drift (total < baseline) also blocks: once a refactor
lowers the count, every subsequent commit is red until
STRUCT_FIELD_BASELINE is lowered to match -- the floor tracks reality,
so the count cannot silently drift below a stale baseline. Strict-pin:
actual must equal baseline; both directions return 1.

For a temporary increase (add sub-structs first, move fields out
after), bump STRUCT_FIELD_BASELINE in the same commit with a reason,
then lower it when the move completes.

The field-counting is shared with report_structure_facts (the L3
report-only detector in make verify); this gate is the L2 blocking
counterpart in make check.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from report_structure_facts import (  # noqa: E402
    FIELD_WARN_THRESHOLD,
    struct_field_counts,
)

STRUCT_FIELD_BASELINE = 517
# 516->517 T31c: Runner +queued_notifications (lower-priority async-completion queue).
# pub(crate) fields drop from the count (regex gap); re-raise when fixed.
# The counter regex only matches private and pub fields, so pub(crate)
# fields drop from the count (known gap); re-raise when the regex is fixed.


def evaluate(total, baseline=STRUCT_FIELD_BASELINE) -> int:
    """Pure strict-pin: 0 only when total == baseline. Growth (>) and
    drift (<) both return 1 -- the floor tracks reality. Pure so the
    regression test can lock both directions without touching the filesystem."""
    return 0 if total == baseline else 1


def main() -> int:
    counts = struct_field_counts()
    total = sum(n for _, n in counts)
    if total > STRUCT_FIELD_BASELINE:
        print(
            f"error: struct field-count ratchet breached: {total} > "
            f"{STRUCT_FIELD_BASELINE} (+{total - STRUCT_FIELD_BASELINE}). A "
            f"God-struct refactor must not increase the total field count — "
            f"split and move fields, do not split and add. If this is a "
            f"temporary refactor state, bump STRUCT_FIELD_BASELINE with a "
            f"reason and lower it when the move completes.",
            file=sys.stderr,
        )
        for name, n in sorted(counts, key=lambda x: -x[1])[:5]:
            print(f"  {n} fields: {name}", file=sys.stderr)
        return 1
    if total < STRUCT_FIELD_BASELINE:
        print(
            f"error: struct field-count ratchet drifted: {total} < "
            f"{STRUCT_FIELD_BASELINE} (-{STRUCT_FIELD_BASELINE - total}). "
            f"Lower STRUCT_FIELD_BASELINE to {total} in this commit so the "
            f"floor tracks reality (strict-pin: drift blocks until fixed).",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
