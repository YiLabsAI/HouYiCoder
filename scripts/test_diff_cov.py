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

Run: python3 scripts/test_diff_cov.py  (wired into make check as
diff-cov-tests). Exit 0 = pass, 1 = fail.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_diff_coverage import (  # noqa: E402
    is_path_rewrite,
    is_macro_rename,
    module_renames,
    parse_added_lines,
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
    # production file in the same diff is kept.
    diff = (
        "+++ b/crates/a/src/dispatch_tests.rs\n"
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

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[diff-cov-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[diff-cov-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
