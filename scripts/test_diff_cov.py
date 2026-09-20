#!/usr/bin/env python3
"""Regression tests for the diff-coverage gate's new-line accounting.

The gate decides which lines a commit must cover. Two failure directions
matter and they are not symmetric: counting a line that is not new code
extorts coverage for a mechanical edit (the noisy direction, which pushes an
author toward padding tests), while failing to count a genuinely new line
passes silently and nobody audits a passing gate. The rename exemption added
to this gate can fail either way, so every test here is PAIRED: the exemption
firing on a real path rewrite is asserted alongside the exemption NOT firing
on a line that changed in any other way.

Covered logics:
  - module_renames accepts only the directory-module shape (src/x_y.rs ->
    src/x/y.rs) and rejects a move whose names do not agree
  - is_path_rewrite is exact: substitution must reproduce the new line byte
    for byte, so a rewritten line plus any logic edit is still counted
  - a path whose prefix is not renamed in this diff is still counted
  - both new spellings of a renamed module are accepted (the full path from
    elsewhere, the bare child name from inside the new parent), while a
    change to a DIFFERENT child is still counted
  - a rewrite the formatter re-wrapped across a different number of lines is
    cleared as one hunk, while a re-wrap carrying a logic edit is counted
  - parse_added_lines only pairs a hunk that replaces n lines with n lines;
    any other shape is counted whole
  - with no renames in the diff nothing is exempt (the exemption cannot fire
    on its own)
  - the IGNORE substring filter still drops test files while keeping
    production files
  - a suspect line table is settled by one rebuild: the table it produced is
    what the verdict must use, clean clears the refusal while still-stale
    keeps it, and a current table spends no rebuild
  - neither a failed rebuild nor a rebuilt report carrying no executable lines
    can clear a refusal: a report cut off after its file headers parses to a
    truthy table whose rows are all empty, so the reader refuses it before any
    measurement, and the report is unlinked either way

Run: python3 scripts/test_diff_cov.py  (wired into make check as
diff-cov-tests). Exit 0 = pass, 1 = fail.
"""
import contextlib
import io
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_diff_coverage as gate  # noqa: E402
from check_diff_coverage import (  # noqa: E402
    is_path_rewrite,
    is_macro_rename,
    module_renames,
    parse_added_lines,
    rebuilt_table,
    settled_verdict,
)

RENAMES = {"projection_memory": "projection::memory"}


def main() -> int:
    failures = []

    # 1. the directory-module shape is accepted; a move whose new name does
    # not agree with the old prefix is not (paired: one input, both verdicts).
    name_status = (
        "R100\tcrates/a/src/projection_memory.rs\tcrates/a/src/projection/memory.rs\n"
        "R100\tcrates/a/src/legacy.rs\tcrates/a/src/other/thing.rs\n"
    )
    got = module_renames(name_status)
    if got != {"projection_memory": "projection::memory"}:
        failures.append(f"module_renames: expected only the directory-module pair, got {got}")

    # 2. a modified file (M, not R) contributes no rename, so its lines stay
    # countable -- the map must come from renames alone.
    got = module_renames("M\tcrates/a/src/server.rs\n")
    if got:
        failures.append(f"module_renames: a modified file is not a rename, got {got}")

    # 3. exact substitution: the pure rewrite is exempt, the same rewrite with
    # an added argument is not.
    old = "    let wire = crate::projection_memory::project(self.list());"
    new = "    let wire = crate::projection::memory::project(self.list());"
    if not is_path_rewrite(old, new, RENAMES):
        failures.append("is_path_rewrite: pure path rewrite must be exempt")
    logic = "    let wire = crate::projection::memory::project(self.list(), true);"
    if is_path_rewrite(old, logic, RENAMES):
        failures.append("is_path_rewrite: rewrite plus a logic edit must be counted")

    # 4. a path change whose prefix is not renamed in this diff is counted --
    # the exemption is scoped to the renames actually in the commit.
    if is_path_rewrite("    TypeA::f(x)", "    TypeB::f(x)", RENAMES):
        failures.append("is_path_rewrite: unrelated path change must be counted")

    # 5. the new parent names its own child bare, so both spellings are
    # accepted; a change naming a DIFFERENT child is not.
    parent_ref = {"command_render": "command::render"}
    if not is_path_rewrite("    x(command_render::f())", "    x(render::f())", parent_ref):
        failures.append("is_path_rewrite: bare child spelling must be exempt")
    if is_path_rewrite("    x(command_render::f())", "    x(worktree::f())", parent_ref):
        failures.append("is_path_rewrite: a different child must be counted")

    # 6. the longer path can push a statement past the line limit, so the
    # formatter re-wraps it and the hunk sides differ in line count. The
    # statement is unchanged, so the hunk clears; the same re-wrap carrying a
    # logic edit does not.
    reflow = (
        "+++ b/crates/a/src/dispatch.rs\n"
        "@@ -10,1 +10,2 @@\n"
        f"-{old}\n"
        "+    let wire =\n"
        "+        crate::projection::memory::project(self.list());\n"
    )
    got = parse_added_lines(reflow, RENAMES)
    if got:
        failures.append(f"parse_added_lines: reflowed rewrite must clear, got {got}")
    reflow_plus_logic = (
        "+++ b/crates/a/src/dispatch.rs\n"
        "@@ -10,1 +10,2 @@\n"
        f"-{old}\n"
        "+    let wire =\n"
        "+        crate::projection::memory::project(self.list(), true);\n"
    )
    got = parse_added_lines(reflow_plus_logic, RENAMES)
    if got != {"crates/a/src/dispatch.rs": {10, 11}}:
        failures.append(f"parse_added_lines: reflow plus logic edit must be counted, got {got}")

    # 7. hunk pairing. Balanced hunk: the rewrite is exempt, the logic edit in
    # the same file is still counted (paired in one diff so a blanket
    # exemption or a blanket count both fail).
    diff = (
        "+++ b/crates/a/src/dispatch.rs\n"
        "@@ -10,1 +10,1 @@\n"
        f"-{old}\n"
        f"+{new}\n"
        "@@ -20,1 +20,1 @@\n"
        "-    let n = 1;\n"
        "+    let n = 2;\n"
    )
    got = parse_added_lines(diff, RENAMES)
    if got != {"crates/a/src/dispatch.rs": {20}}:
        failures.append(f"parse_added_lines: expected only line 20 counted, got {got}")

    # 8. unbalanced hunk that is not a re-wrap: one line replaced by the
    # rewrite plus a genuinely new line. The statement no longer matches, so
    # nothing is exempt and both lines count.
    diff = (
        "+++ b/crates/a/src/dispatch.rs\n"
        "@@ -10,1 +10,2 @@\n"
        f"-{old}\n"
        f"+{new}\n"
        "+    let extra = 1;\n"
    )
    got = parse_added_lines(diff, RENAMES)
    if got != {"crates/a/src/dispatch.rs": {10, 11}}:
        failures.append(f"parse_added_lines: unbalanced hunk must count both lines, got {got}")

    # 9. with no renames in the diff the exemption cannot fire.
    diff = (
        "+++ b/crates/a/src/dispatch.rs\n"
        "@@ -10,1 +10,1 @@\n"
        f"-{old}\n"
        f"+{new}\n"
    )
    got = parse_added_lines(diff, {})
    if got != {"crates/a/src/dispatch.rs": {10}}:
        failures.append(f"parse_added_lines: no renames means nothing exempt, got {got}")

    # 10. the IGNORE filter survives the rewrite: a test file is dropped, a
    # production file in the same diff is kept. Both test-file shapes count:
    # the flat X_tests.rs and a submodule under the X_tests/ directory.
    diff = (
        "+++ b/crates/a/src/dispatch_tests.rs\n"
        "@@ -1,0 +1,1 @@\n"
        "+    assert!(true);\n"
        "+++ b/crates/a/src/dispatch_tests/helper.rs\n"
        "@@ -1,0 +1,1 @@\n"
        "+    assert!(true);\n"
        "+++ b/crates/a/src/dispatch.rs\n"
        "@@ -5,0 +5,1 @@\n"
        "+    let n = 2;\n"
    )
    got = parse_added_lines(diff, RENAMES)
    if got != {"crates/a/src/dispatch.rs": {5}}:
        failures.append(f"parse_added_lines: IGNORE must drop only the test file, got {got}")

    # 11. a line whose only change is swapping eprintln! for tracing::warn!
    # carries no new logic, so it is not counted as new code. Paired: a line
    # that also changed the message IS counted.
    if not is_macro_rename(
        'eprintln!("failed: {e}");', 'tracing::warn!("failed: {e}");'
    ):
        failures.append("is_macro_rename: pure prefix swap must be exempt")
    if is_macro_rename(
        'eprintln!("failed: {e}");', 'tracing::warn!("different message");'
    ):
        failures.append("is_macro_rename: message change must NOT be exempt")

    # 12. a suspect line table is settled by one rebuild, and the table the
    # verdict must use is what comes back -- the rebuilt one, never the table
    # this call just called untrustworthy. Paired on one entry point: a clean
    # rebuild clears the refusal, a still-stale one keeps it, and reverting
    # either half (returning the suspect table, or keeping the evidence of a
    # settled table) turns one of these red.
    SUSPECT_TABLE = {"a.rs": {9: True}}
    SUSPECT = ["a.rs:9 (file has 8 lines)"]
    FRESH = {"a.rs": {1: True}}
    calls = []

    def rebuild(table):
        def go():
            calls.append(1)
            return table

        return go

    def measure_fresh_only(table):
        return [] if table is FRESH else ["measured the table that prompted the rebuild"]

    got, evidence, rebuilt = settled_verdict(
        SUSPECT_TABLE, SUSPECT, rebuild(FRESH), measure_fresh_only
    )
    if got is not FRESH or evidence != [] or rebuilt is not True or len(calls) != 1:
        failures.append(f"settled_verdict: a clean rebuilt table must be the one used, got {got}")
    got, evidence, rebuilt = settled_verdict(
        SUSPECT_TABLE, SUSPECT, rebuild(FRESH), lambda _t: SUSPECT
    )
    if got is not FRESH or evidence != SUSPECT or rebuilt is not True:
        failures.append(f"settled_verdict: a stale rebuilt table must still refuse, got {evidence}")

    # 13. a current table spends no rebuild -- this branch must stay off the
    # normal path, where a rebuild costs a full instrumented compile -- and the
    # caller's own table stays the one the verdict is drawn from.
    spent = []

    def never():
        spent.append(1)
        return FRESH

    got, evidence, rebuilt = settled_verdict(FRESH, [], never, lambda _t: [])
    if got is not FRESH or evidence != [] or rebuilt is not False or spent:
        failures.append(f"settled_verdict: a current table must not rebuild, spent {spent}")

    # 14. a rebuild that produced no table leaves both the evidence and the
    # refusal standing, and it is caught before the measurement: an empty
    # table has no position past end-of-file, so measuring it would read as
    # clean and clear a refusal the rebuild never settled.
    measured = []
    got, evidence, rebuilt = settled_verdict(
        SUSPECT_TABLE, SUSPECT, lambda: {}, lambda t: measured.append(t) or []
    )
    if got is not SUSPECT_TABLE or evidence != SUSPECT or rebuilt is not False or measured:
        failures.append(f"settled_verdict: an empty rebuilt table must refuse, got {evidence}")
    got, evidence, rebuilt = settled_verdict(SUSPECT_TABLE, SUSPECT, lambda: None, lambda _t: [])
    if got is not SUSPECT_TABLE or evidence != SUSPECT or rebuilt is not False:
        failures.append(f"settled_verdict: a failed rebuild must not clear it, got {evidence}")
    # A rebuild that raises must land the same way as one that returned nothing:
    # the refusal stands, and the exception must not escape -- a traceback exits
    # 1, which is not the gate's refusal exit. The redirect keeps the deliberate
    # traceback off this test's own output.
    def boom():
        raise OSError("the instrumented run fell over")

    with contextlib.redirect_stderr(io.StringIO()):
        got, evidence, rebuilt = settled_verdict(
            SUSPECT_TABLE, SUSPECT, boom, lambda _t: []
        )
    if got is not SUSPECT_TABLE or evidence != SUSPECT or rebuilt is not False:
        failures.append(f"settled_verdict: a raising rebuild must refuse, got {evidence}")
    # Headers without a DA line parse to keys whose every row is empty, which is
    # truthy: the guard must read the rows, not the table.
    got, evidence, rebuilt = settled_verdict(
        SUSPECT_TABLE, SUSPECT, lambda: {"crates/a/src/lib.rs": {}}, lambda t: measured.append(t) or []
    )
    if got is not SUSPECT_TABLE or evidence != SUSPECT or rebuilt is not False or measured:
        failures.append(f"settled_verdict: a rebuilt table with empty rows must refuse, got {evidence}")

    # 15. the reader of a rebuilt report refuses one whose file entries carry no
    # executable lines. The parse gives an empty row per file header, which is a
    # truthy table with no position that can disagree with anything, so a report
    # cut off after its headers would clear the refusal the rebuild was spent to
    # settle. The control is the same report one DA line longer.
    original = gate.instrumented_report
    written = []

    def report_of(body: str):
        def write(cov_dir, rebuild):
            fd, name = tempfile.mkstemp(suffix=".lcov")
            os.close(fd)
            path = Path(name)
            path.write_text(body, encoding="utf-8")
            written.append(path)
            return path

        return write

    try:
        gate.instrumented_report = report_of("SF:crates/a/src/lib.rs\nend_of_record\n")
        before = len(written)
        if rebuilt_table("cov") is not None:
            failures.append("rebuilt_table: a report with no DA lines must read as no table")
        # The patch must be the reader that ran: without this the case would
        # pass on an unreached writer and fall through to a real instrumented
        # build.
        if len(written) != before + 1:
            failures.append("rebuilt_table: the patched rebuild was not reached")
        elif written[-1].exists():
            failures.append("rebuilt_table: the rebuilt report must be removed")

        gate.instrumented_report = report_of("SF:crates/a/src/lib.rs\nDA:1,1\nend_of_record\n")
        if rebuilt_table("cov") != {"crates/a/src/lib.rs": {1: True}}:
            failures.append("rebuilt_table: a report with a DA line must yield that table")
        if written[-1].exists():
            failures.append("rebuilt_table: the report is removed on the reading path too")

        gate.instrumented_report = lambda cov_dir, rebuild: None
        if rebuilt_table("cov") is not None:
            failures.append("rebuilt_table: a failed rebuild must read as no table")
    finally:
        gate.instrumented_report = original

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[diff-cov-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[diff-cov-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
