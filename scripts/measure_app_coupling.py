#!/usr/bin/env python3
"""Measure App coupling for the God-struct split feasibility.

CORRECT method (default-include, explicit-exclude): collect EVERY
app.<ident> and self.<ident> in each &mut App fn body -- fields AND
method calls -- then categorize. The residue (idents NOT in the target
cluster) is what blocks narrowing. This is the opposite of the flawed
method that only counted cluster-dict fields (silently dropping unknowns
-- the hole that made the permission-split premise look viable when 61%
of fns call App methods like system_line / send_cmd / mint_request_id).

A fn narrows to &mut <Sub> ONLY IF its residue is empty (touches only
the target cluster's fields, no App methods, no other fields). Any
method call or non-cluster field is a coupling point that blocks
narrowing unless that method/field also moves.

categorize_touches is pure (testable); scan_files is the driver.
Regression-tested via test_app_coupling.py.
"""
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent / "crates/houyicoder-tui/src"

CLUSTER_FIELDS = frozenset({
    "approval", "pending_approvals", "pending_permission_req_id",
    "mode_cache", "rules_cache", "dirs_cache", "ask_before_git_enabled",
    "sticky_choices", "verdict_log_cache", "verdict_cursor",
    "permission_tab", "permission_cursor", "permission_input",
    "permission_search", "working_dir",
})

IDENT_RE = re.compile(r"(?:app|self)\.(\w+)")
MUT_APP_RE = re.compile(r"&\s*mut\s+App\b")


def categorize_touches(body, cluster=CLUSTER_FIELDS):
    """Return (cluster_touches, residue) for a fn body. Default-include
    every app.<ident>/self.<ident> (fields + method calls); explicit-
    exclude cluster fields. Residue = methods + non-cluster fields -- the
    coupling points that block narrowing. Pure; tested by
    test_app_coupling."""
    touched = {m.group(1) for m in IDENT_RE.finditer(body)}
    return touched & cluster, touched - cluster


def _fns_taking_mut_app(text):
    for m in re.finditer(r"\bfn\s+(\w+)\s*\(", text):
        j = text.find("{", m.end())
        if j < 0:
            continue
        if not MUT_APP_RE.search(text[m.start():j]):
            continue
        depth, k, n = 0, j, len(text)
        while k < n:
            c = text[k]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    break
            k += 1
        yield m.group(1), text[j + 1:k]


def scan_files():
    rows = []
    for f in sorted(REPO.rglob("*.rs")):
        try:
            text = f.read_text(encoding="utf-8")
        except OSError:
            continue
        rel = str(f.relative_to(REPO.parent))
        for name, body in _fns_taking_mut_app(text):
            cluster_hits, residue = categorize_touches(body)
            rows.append((rel, name, cluster_hits, residue))
    return rows


def main():
    rows = scan_files()
    n_narrows = n_blocked = 0
    print(f"{'fn':45} {'cluster':7} {'residue':7} verdict")
    for rel, name, cluster_hits, residue in rows:
        c, r = len(cluster_hits), len(residue)
        if r == 0 and c > 0:
            v = "-> narrows to &mut <Sub>"
            n_narrows += 1
        else:
            v = f"BLOCKED by: {', '.join(sorted(residue)[:5])}" if r else "(no cluster touch)"
            n_blocked += 1
        print(f"{name:45} {c:7} {r:7} {v}")
    print(f"\nnarrows: {n_narrows} | blocked: {n_blocked} | total: {len(rows)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
