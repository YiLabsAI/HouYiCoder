#!/usr/bin/env python3
"""Diff coverage gate: percent of new/modified lines covered by unit tests.

A line whose only change is a module path renamed elsewhere in the same diff
is not new code and is left out of the measured set; see is_path_rewrite.

Unit-only: runs `cargo llvm-cov --lib` so integration tests (tests/) do not
count toward coverage (they validate end-to-end paths, not unit isolation).
The gate fails when the coverage of NEW or MODIFIED lines (the diff vs
COV_BASE, default HEAD for pre-commit; set origin/main for CI) drops below
COV_DIFF_THRESHOLD (85 starter, raise to 90 once green).

Why diff coverage as well as a total: the workspace gate (check_coverage.sh)
holds a floor on the codebase as a whole, where one module can regress several
points without moving the number; this gate forces NEW code to be tested as it
lands, which is where a regression actually enters.

Skipped (warn) if cargo-llvm-cov is not installed -- opt-in until the env
has it, same as check_coverage.sh.
"""
from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# Shared lcov parse + stale-mapping detect + reject, used by both this gate
# and check_coverage.sh so a stale line-table cannot pass either consumer.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from cov_lcov import normalize, lcov_executable_lines, stale_mapping_evidence  # noqa: E402

THRESHOLD = int(os.environ.get("COV_DIFF_THRESHOLD", "85"))
BASE = os.environ.get("COV_BASE", "HEAD")
# Colon-separated substrings; a diff file whose path contains any is ignored
# (it is a test, not production code to cover, or a stub/exempt crate). Add
# your own via COV_IGNORE="foo:bar"; the defaults are appended.
IGNORE = os.environ.get(
    "COV_IGNORE",
    "_tests.rs:/tests/:houyicoder-cli:houyicoder-graph:houyicoder-wasm",
).split(":")
ROOT = Path(__file__).resolve().parent.parent


def run(cmd, **kw):
    return subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, **kw)


# --- 1. added/modified line numbers in the diff vs BASE, crates/**/*.rs only ---
HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")


def module_renames(name_status: str) -> dict[str, str]:
    """Map each flat-prefix module renamed in this diff to its new path.

    A rename of src/x_y.rs to src/x/y.rs turns every reference x_y::item into
    x::y::item. The pair is recognized only when the new file sits in a
    directory named after the old prefix and the two names agree exactly, so
    the map holds mechanical directory-module moves and nothing else. Input is
    the output of git diff --name-status -M.
    """
    renames: dict[str, str] = {}
    for line in name_status.splitlines():
        parts = line.split("\t")
        if len(parts) != 3 or not parts[0].startswith("R"):
            continue
        old, new = Path(parts[1]), Path(parts[2])
        if old.suffix != ".rs" or new.suffix != ".rs":
            continue
        parent = new.parent.name
        if old.stem == f"{parent}_{new.stem}" and old.parent == new.parent.parent:
            renames[old.stem] = f"{parent}::{new.stem}"
    return renames


def path_variants(text: str, renames: dict[str, str]) -> list[str]:
    """Every rewriting of text that the given module renames can produce.

    A reference to a renamed module has two legal new spellings, and which one
    a call site uses depends on where it sits: a file elsewhere in the crate
    names the full new path x::y, while the new parent x.rs names its own child
    as just y. Each renamed module appearing in the text contributes that
    choice, so the variants are the product of the choices.
    """
    present = [f for f in renames if re.search(rf"\b{re.escape(f)}::", text)]
    if not present:
        return []
    variants = [text]
    for flat in present:
        nested = renames[flat]
        options = {nested, nested.rsplit("::", 1)[-1]}
        variants = [
            re.sub(rf"\b{re.escape(flat)}::", f"{opt}::", v)
            for v in variants
            for opt in options
        ]
    return variants


def is_path_rewrite(old: str, new: str, renames: dict[str, str]) -> bool:
    """True when new is old with only renamed module paths substituted.

    Such a line carries no new logic: it names the same items by their new
    path. Counting it as new code would charge a mechanical rename for unit
    coverage the line never had, so the diff set leaves it out. The judgement is
    exact rather than a heuristic -- only modules actually renamed in this diff
    are substituted, and some rewriting of the old text must equal the new text
    exactly, so text that also changed in any other way is still counted.
    """
    return any(v == new for v in path_variants(old, renames))


# Macro-name replacements that carry no new logic. A line whose only change
# is swapping one of these prefixes for another is a mechanical migration
# (the message and the error binding are the same), not new code to cover.
# The old line was never covered either -- it sat in an error branch -- so
# charging the replacement for coverage would block a migration on tests
# that cannot be written for the original either.
_MACRO_RENAMES: list[tuple[str, str]] = [
    ("eprintln!", "tracing::warn!"),
    ("eprintln!", "tracing::debug!"),
    ("eprintln!", "tracing::error!"),
    ("eprintln!", "tracing::info!"),
    ("eprintln!", "tracing::trace!"),
]


def is_macro_rename(old: str, new: str) -> bool:
    """True when new is old with only a known macro prefix swapped.

    Same principle as is_path_rewrite: the line carries no new logic, only
    a different sink for the same message. The coverage the line never had
    should not suddenly be required because the sink changed.
    """
    for old_pfx, new_pfx in _MACRO_RENAMES:
        if old_pfx in old and new_pfx in new:
            stripped_old = old.replace(old_pfx, new_pfx, 1)
            if stripped_old == new:
                return True
    return False


def collapse(lines: list[str]) -> str:
    """Join lines and squeeze runs of whitespace into single spaces.

    A new path can be longer than the one it replaces, which pushes the
    statement past the line limit and makes the formatter wrap it. The result is
    a hunk whose sides have different line counts even though the statement is
    unchanged apart from the path, so the comparison is made on the statement
    rather than on the lines it happens to occupy.
    """
    return " ".join(" ".join(lines).split())


def parse_added_lines(diff: str, renames: dict[str, str]) -> dict[str, set[int]]:
    """Return {normalized_file: set(new_line_numbers)} for added lines in diff."""
    added: dict[str, set[int]] = {}
    cur_file = None
    new_line = 0
    removed: list[str] = []
    inserted: list[tuple[int, str]] = []

    def flush() -> None:
        # Judge the hunk as a whole first, which is the only way to clear a
        # rewrite the formatter then re-wrapped across a different number of
        # lines. Failing that, a hunk replacing n lines with n lines pairs them
        # in order so each new line is judged against its one predecessor; any
        # other shape is counted whole.
        whole = is_path_rewrite(
            collapse(removed), collapse([t for _, t in inserted]), renames
        )
        paired = len(removed) == len(inserted)
        for i, (ln, text) in enumerate(inserted):
            if whole or (paired and is_path_rewrite(removed[i], text, renames)):
                continue
            if paired and is_macro_rename(removed[i], text):
                continue
            added.setdefault(cur_file, set()).add(ln)
        removed.clear()
        inserted.clear()

    for line in diff.splitlines():
        if line.startswith("+++ b/"):
            flush()
            f = normalize(line[6:])
            # Skip files that match an IGNORE substring (test files, stub or
            # exempt crates) -- they are not production code to cover.
            cur_file = None if any(x in f for x in IGNORE) else f
            continue
        if line.startswith("--- "):
            continue
        m = HUNK_RE.match(line)
        if m:
            flush()
            new_line = int(m.group(1))
            continue
        if cur_file is None:
            continue
        if line.startswith("+"):
            inserted.append((new_line, line[1:]))
            new_line += 1
        elif line.startswith("-"):
            # removed line (old side); new-side line counter does not advance
            removed.append(line[1:])
        else:
            # context line (--unified=0 emits none, but be safe)
            new_line += 1
    flush()
    return {f: s for f, s in added.items() if s}


def untracked_listing() -> str:
    """The untracked, non-ignored paths under crates, as git reports them."""
    res = run(["git", "ls-files", "--others", "--exclude-standard", "--", "crates"])
    if res.returncode != 0:
        print(f"error: git ls-files --others failed: {res.stderr.strip()}", file=sys.stderr)
        sys.exit(2)
    return res.stdout


def untracked_sources(listing: str) -> list[str]:
    """The normalized source paths in an ls-files listing that this gate must
    account for: Rust files, minus the same exemptions the diff path applies.

    Split from reading the files so the selection rule can be checked on a
    string, like every other rule in this module. Choosing the paths is where
    a mistake hides (a missed suffix, an exemption that stops matching after
    normalization); counting the lines of a chosen file is not.
    """
    out: list[str] = []
    for rel in listing.split():
        if not rel.endswith(".rs"):
            continue
        f = normalize(rel)
        if any(x in f for x in IGNORE):
            continue
        out.append(f)
    return out


def untracked_added_lines(listing: str, root: Path = ROOT) -> dict[str, set[int]]:
    """Return {normalized_file: every line number} for untracked source files.

    A file git does not track yet is absent from git diff, so without this it
    contributes no lines and a brand-new module reads as fully covered. That
    inverts the gate exactly where it matters most: the percentage comes out
    highest when a whole new file has no tests at all, and the percentage is
    what a reader trusts.

    The whole file counts as added, which is what it is. Non-executable lines
    drop out later anyway, when this set is intersected with the executable
    lines the coverage report lists. A path that has since disappeared is
    skipped: git listed it, a concurrent edit removed it, and neither is this
    gate's business to complain about.

    check_code.sh already unions ls-files --others into the file list it
    passes the comment and naming gates, for the same reason and after the
    same kind of miss. This is that fix, applied to the gate that still had
    the hole.
    """
    out: dict[str, set[int]] = {}
    for f in untracked_sources(listing):
        try:
            n = len((root / f).read_text(encoding="utf-8").splitlines())
        except OSError:
            continue
        if n:
            out[f] = set(range(1, n + 1))
    return out


def diff_added_lines():
    """Return {normalized_file: set(new_line_numbers)} for added lines vs BASE,
    including files that are not tracked yet."""
    res = run(["git", "diff", "--unified=0", BASE, "--", "crates"])
    if res.returncode != 0:
        print(f"error: git diff vs {BASE} failed: {res.stderr.strip()}", file=sys.stderr)
        sys.exit(2)
    names = run(["git", "diff", "--name-status", "-M", BASE, "--", "crates"])
    if names.returncode != 0:
        print(f"error: git diff --name-status vs {BASE} failed: {names.stderr.strip()}", file=sys.stderr)
        sys.exit(2)
    added = parse_added_lines(res.stdout, module_renames(names.stdout))
    for f, lines in untracked_added_lines(untracked_listing()).items():
        added.setdefault(f, set()).update(lines)
    return added


# --- 2. executable lines per file from the lcov report (DA:line,hit) ---


def drop_profraw(cov_dir: str) -> None:
    """Delete raw coverage samples under the given directory.

    The instrumented run keeps its build cache on purpose, but the same flag
    leaves the samples, and the report merges every one it finds. A sample from
    an earlier revision carries that revision's line numbers, so merging it
    attributes hits to whatever now occupies those lines.
    """
    for root, _dirs, files in os.walk(cov_dir):
        for name in files:
            if name.endswith(".profraw"):
                try:
                    os.remove(os.path.join(root, name))
                except OSError:
                    pass


def instrumented_report(cov_dir: str, rebuild: bool) -> Path | None:
    """Run the instrumented suite and return the report it writes.

    With rebuild set, the whole instrumented tree goes first. That is the only
    way to be sure of the line table, because the table is compiled into the
    artifacts rather than derived at report time, so artifacts left from before
    an edit describe the file as it used to be. Keeping them is what makes the
    usual path fast, and is right until a verdict depends on them being current.
    """
    if rebuild:
        shutil.rmtree(os.path.join(ROOT, cov_dir), ignore_errors=True)
    else:
        drop_profraw(cov_dir)
    with tempfile.NamedTemporaryFile(suffix=".lcov", delete=False) as tf:
        out = Path(tf.name)
    env = {**os.environ, "HOUYICODER_FAST_TOKENS": "1", "CARGO_TARGET_DIR": cov_dir}
    cov = subprocess.run(
        ["cargo", "llvm-cov", "--no-clean", "--lib", "--workspace", "--lcov",
         "--output-path", str(out)],
        cwd=ROOT, capture_output=True, text=True, env=env,
    )
    if cov.returncode != 0:
        print(f"error: cargo llvm-cov failed: {cov.stderr.strip()}", file=sys.stderr)
        out.unlink(missing_ok=True)
        return None
    return out


def tally(added: dict, executable: dict) -> tuple[int, int, list[str]]:
    """Count how many of the added lines the report covers.

    A line the report does not mention is skipped rather than counted as
    uncovered: attributes, declarations, comments and blanks compile to nothing,
    so they are not coverable and holding them against the diff would make the
    percentage a function of comment density.
    """
    covered = 0
    counted = 0
    missing: list[str] = []
    for path, lines in added.items():
        file_exec = executable.get(path, {})
        for ln in sorted(lines):
            if ln not in file_exec:
                continue
            counted += 1
            if file_exec[ln]:
                covered += 1
            elif len(missing) < 20:
                missing.append(f"{path}:{ln}")
    return covered, counted, missing


def main() -> int:
    if not run(["sh", "-c", "command -v cargo-llvm-cov"]).stdout.strip():
        print("warn: cargo-llvm-cov not installed; diff coverage gate skipped.", file=sys.stderr)
        print("      install: rustup component add llvm-tools-preview && cargo install cargo-llvm-cov", file=sys.stderr)
        return 0

    added = diff_added_lines()
    if not added:
        print(f"ok: no new/modified lines vs {BASE}; nothing new to gate.")
        return 0

    total_added = sum(len(s) for s in added.values())
    if total_added == 0:
        print(f"ok: no new/modified lines vs {BASE}.")
        return 0

    # Isolate the cov build cache (target/cov) so the instrumented build
    # does not thrash the plain dev cache (target/). See run_tests.py
    # COV_TARGET_DIR. The lcov report lives in the same cov dir.
    COV_DIR = os.path.join("target", "cov")
    LCOV = os.path.join(COV_DIR, "houyi-cov.lcov")
    created_temp = False

    def lcov_is_fresh(lcov_path: Path) -> bool:
        """True if the lcov is newer than every .rs source file under
        crates/, so an edit since the last coverage run invalidates the
        cache. Avoids the bug where a stale lcov (from a run before the
        source changed) is reused + reports the old coverage as if fresh.
        Over-invalidates (any .rs edit, even a comment) forces a re-run;
        that is safe -- just a slower diff-cov, not a wrong result.

        Freshness is about the report, not about the samples behind it. A report
        rewritten from samples left by an earlier revision passes this check on
        its timestamp while carrying that revision's line numbers, so the sample
        files are deleted before every instrumented run rather than judged here.
        """
        try:
            lcov_mtime = lcov_path.stat().st_mtime
        except OSError:
            return False
        for rs in ROOT.glob("crates/**/*.rs"):
            try:
                if rs.stat().st_mtime > lcov_mtime:
                    return False
            except OSError:
                continue
        return True

    if Path(LCOV).is_file() and lcov_is_fresh(Path(LCOV)):
        # Reuse the trace run_tests.py produced in the test step -- avoids a
        # second instrumented compile in make check. The freshness check
        # guarantees it covers the current source (no stale-lcov reuse).
        lcov_path = Path(LCOV)
    else:
        if not run(["sh", "-c", "command -v cargo-llvm-cov"]).stdout.strip():
            print("warn: cargo-llvm-cov not installed and no cached lcov; diff coverage gate skipped.", file=sys.stderr)
            return 0
        if Path(LCOV).is_file():
            print("note: cached lcov is stale vs source; re-running cargo llvm-cov.", file=sys.stderr)
        # Unit-only (--lib): integration tests do not count toward coverage.
        fresh = instrumented_report(COV_DIR, rebuild=False)
        if fresh is None:
            return 2
        lcov_path = fresh
        created_temp = True

    executable = lcov_executable_lines(lcov_path)
    if created_temp:
        lcov_path.unlink(missing_ok=True)

    stale = stale_mapping_evidence(executable)
    if stale:
        print(
            "error: the coverage report describes source that is not the source on "
            "disk, so no verdict can be drawn from it.",
            file=sys.stderr,
        )
        print(
            "  Positions it reports that lie past the end of the file:",
            file=sys.stderr,
        )
        for s in stale[:5]:
            print(f"    {s}", file=sys.stderr)
        print(
            "  The line table comes from the instrumented binary, so this means the\n"
            "  binary predates an edit. Delete target/cov and re-run to rebuild it.",
            file=sys.stderr,
        )
        return 2

    covered_added, counted_added, missing = tally(added, executable)
    if counted_added == 0:
        print(f"ok: no new executable lines vs {BASE}.")
        return 0
    pct = 100.0 * covered_added / counted_added

    # A failing verdict gets one recheck against a rebuilt line table before it
    # is believed. The table is compiled into the instrumented artifacts, so
    # artifacts kept from before an edit describe the file as it used to be, and
    # the resulting report attributes hits to whatever now sits at those numbers.
    # Observed shape: lines that are now comments are reported as uncovered code,
    # which no test can fix. The rebuild is slow, which is why it is spent only
    # when the answer would otherwise be a refusal.
    #
    # This protects the failing direction only. The same staleness can hide a
    # genuinely uncovered line by crediting it with a hit that belonged to
    # earlier code, and that direction passes silently and is not rechecked here.
    if pct + 1e-9 < THRESHOLD:
        print(
            f"note: {pct:.1f}% is below the threshold; rebuilding the instrumented "
            "tree to rule out a stale line table before failing.",
            file=sys.stderr,
        )
        rebuilt = instrumented_report(COV_DIR, rebuild=True)
        if rebuilt is not None:
            executable = lcov_executable_lines(rebuilt)
            rebuilt.unlink(missing_ok=True)
            covered_added, counted_added, missing = tally(added, executable)
            if counted_added == 0:
                print(f"ok: no new executable lines vs {BASE}.")
                return 0
            pct = 100.0 * covered_added / counted_added

    print(f"diff coverage vs {BASE}: {covered_added}/{counted_added} new executable lines = {pct:.1f}% (threshold {THRESHOLD}%)")
    if missing:
        print("uncovered new lines (first 20):", file=sys.stderr)
        for m in missing:
            print(f"  {m}", file=sys.stderr)
    return 1 if pct + 1e-9 < THRESHOLD else 0


if __name__ == "__main__":
    sys.exit(main())
