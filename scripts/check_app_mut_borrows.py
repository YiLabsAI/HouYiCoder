#!/usr/bin/env python3
"""&mut App ratchet: the number of functions that take
&mut App as a parameter must not increase above the baseline.

The acceptance gate for the App God-struct refactor. The field-count
ratchet (check_struct_fields) blocks App from GROWING fields; this
ratchet blocks the broad-access surface from GROWING. Together they
catch both failure modes of a God-struct refactor:
  - split a struct + sneak in new fields   -> struct-fields (field count)
  - narrow a sig to &mut Sub but re-wrap a
    &mut App inside, so access stays broad -> this ratchet (access surface)

A &mut App parameter is "broad access" -- the body can touch any App
field; the type carries no narrowing. The refactor replaces these with
&mut <Sub> so the access surface shows in the type. Each narrowing
lowers the count; the baseline is lowered in the same commit so the
ratchet stays tight.

The count bottoms out at the dispatcher/renderer floor -- functions
whose job is to fan out across many App fields (input dispatch,
full-screen draw). Those stay &mut App; this ratchet does NOT force
splitting them. There is no hard target threshold, only "must not
increase": the baseline sinks as sigs narrow and stops at the floor.
At the floor the ratchet still blocks NEW &mut App functions; adding a
legitimate new dispatcher requires bumping MUT_APP_BASELINE with a
reason, visible in the diff.

The floor (the dispatchers/renderers that stay broad-access) is kept in
the count, not subtracted here. Subtracting an exempt set would let
"reclassify a fn as dispatcher" lower the count without any real narrowing
-- the exact fake-narrowing this gate exists to catch. Keeping the floor in
the count itself makes it visible.
"""
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.paths import is_test_file

# Production &mut App fn count. Test files excluded: a test helper
# legitimately mutates the whole App, so counting it trips the gate on
# every new test. Measured 2026-08-17.
MUT_APP_BASELINE = 39


def evaluate(total, baseline=MUT_APP_BASELINE) -> int:
    """Pure strict-pin: 0 only when total == baseline. Growth (>) and
    drift (<) both return 1 -- the floor tracks reality. Pure so the
    regression test can lock both directions without scanning source."""
    return 0 if total == baseline else 1


def _sig_has_mut_app_param(src: str, fn_pos: int) -> bool:
    """True if the parameter list (the first (...) after the fn name)
    contains &mut App. Scans to the first '(' then collects to its
    matching ')' so multi-line signatures are captured and the return
    type is ignored."""
    i = fn_pos
    n = len(src)
    while i < n and src[i] != "(":
        i += 1
    if i >= n:
        return False
    depth = 0
    params: list[str] = []
    while i < n:
        c = src[i]
        if c == "(":
            depth += 1
            if depth == 1:
                i += 1
                continue
        elif c == ")":
            depth -= 1
            if depth == 0:
                break
        if depth >= 1:
            params.append(c)
        i += 1
    return "&mut App" in "".join(params)


def mut_app_counts(root: Path) -> list[tuple[str, int]]:
    repo_root = Path(__file__).resolve().parents[1]
    counts: dict[str, int] = {}
    for f in sorted(root.rglob("*.rs")):
        if is_test_file(str(f.resolve().relative_to(repo_root))):
            continue
        try:
            src = f.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        n = sum(
            1 for m in re.finditer(r"\bfn\s+\w+", src)
            if _sig_has_mut_app_param(src, m.start())
        )
        if n:
            counts[str(f.resolve().relative_to(repo_root))] = n
    return sorted(counts.items(), key=lambda x: -x[1])


def main() -> int:
    tui = (
        Path(__file__).resolve().parents[1]
        / "crates"
        / "houyicoder-tui"
        / "src"
    )
    counts = mut_app_counts(tui)
    total = sum(n for _, n in counts)
    if total > MUT_APP_BASELINE:
        print(
            f"error: &mut App ratchet breached: {total} > "
            f"{MUT_APP_BASELINE} (+{total - MUT_APP_BASELINE}). A "
            f"God-struct refactor must narrow &mut App sigs to &mut <Sub>, "
            f"not add new broad-access ones. If a new dispatcher "
            f"legitimately needs broad access, bump MUT_APP_BASELINE with "
            f"a reason.",
            file=sys.stderr,
        )
        for name, n in sorted(counts, key=lambda x: -x[1])[:5]:
            print(f"  {n}: {name}", file=sys.stderr)
        return 1
    if total < MUT_APP_BASELINE:
        print(
            f"error: &mut App ratchet drifted: {total} < "
            f"{MUT_APP_BASELINE} (-{MUT_APP_BASELINE - total}). Lower "
            f"MUT_APP_BASELINE to {total} in this commit so the floor "
            f"tracks reality (strict-pin: drift blocks until fixed).",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
