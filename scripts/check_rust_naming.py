#!/usr/bin/env python3
"""Rust naming gate: short, segment-bounded test names, plus fn/type length
warns.

A test fn is identified by its #[test] / #[tokio::test] attribute. It
MUST carry a `test_` name prefix — the repo convention, 100% enforced
since the prefix-normalize pass (a `#[test] fn foo()` fails this gate).
The attribute marks the fn as a test to rustc; the prefix marks it to
grep + test-output readers and keeps the suite uniform. The gate also
enforces segment/length caps on every test fn.

Case conventions (snake_case fns, UpperCamelCase types) are already enforced
by rustc lints under -D warnings (non_snake_case, non_camel_case_types); this
script does not duplicate them. It enforces what rustc does not: test-name
segment count and length, and soft length warns on any fn or type name.

Checks .rs files (pass files as args; no args = all crates/**/*.rs):

1. ERROR - a test fn (any #[test] / #[tokio::test]-marked fn) whose full
   name contains more than 4 underscores. Long sentence-style names are hard
   to scan in test output and to filter; move detail into a module doc.
2. ERROR - a test fn name longer than 50 chars (hard).
3. WARN  - a test fn name longer than 40 chars (trim in the same patch).
4. ERROR - a test fn name starting with a project-specific bugN_ prefix;
   names must describe behavior, not reference an internal bug number.
5. ERROR - a test fn name ending on a function word that takes a complement
   (and, after, with, to, is, ...), which means the phrase never closes:
   test_mandatory_deny_read_and has lost its object. These come from trimming
   an over-cap name by dropping trailing segments, which satisfies rule 1
   while destroying the sentence -- rule 1 counts separators and cannot see a
   severed clause. Rename for the behavior; do not cut the last word.
6. WARN  - any non-test fn name longer than 50 chars, or any
   struct/enum/trait/union name longer than 40 chars.
7. ERROR - a struct/enum/trait/union name ending in "Wire". Wire DTOs share
   a name with their domain type and rely on the module path to disambiguate
   (e.g. frontend::memory::MemoryDetail vs context::MemoryEntry); a Wire
   suffix is a smell that the name collides or the type was not given a
   descriptive one. Pick a descriptive name or use the path.
8. ERROR - a flat-prefix module pair: a directory under crates/*/src/ that
   holds both X.rs and X_<suffix>.rs is faking a module hierarchy by filename
   prefix. Rust has a first-class directory module -- X.rs + X/<suffix>.rs
   gives the path X::<suffix>, which does not stutter (X_<suffix>::item
   repeats the X concept twice) and lets the file tree show the subsystem.
   The pair is matched by longest existing parent stem (so markdown_memory_io.rs
   is a child of markdown_memory.rs, not of a nonexistent markdown.rs), and
   both _tests.rs peers are excluded: foo.rs + foo_tests.rs is the repo's
   one-source-one-peer-test convention, and a parent ending in _tests is a
   test-file family, not a source hierarchy. Integration tests (tests/) are
   excluded entirely -- each .rs there is its own binary, not a module.

   Pre-existing pairs are grandfathered in _FLAT_PREFIX_BASELINE so the gate
   ratchets rather than red-ing the whole repo at once: a NEW flat prefix not
   in the baseline is an ERROR, an existing one prints as tolerated stock.
   Remove a path from the baseline in the same commit that converts the pair
   to a directory module, so the count ratchets down toward zero.

Exit 1 on ERROR, 0 otherwise. WARNs print but never fail the gate.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.naming import JARGON, TEST_NAME_LEN_HARD, TEST_UNDERSCORE_CAP  # noqa: E402

# A test attribute: #[test] or #[tokio::test] (optionally with args, e.g.
# #[tokio::test(flavor = "multi_thread")]). Matched anywhere on the line so
# it also catches #[cfg(test)] #[tokio::test] stacking on one line.
_TEST_ATTR_RE = re.compile(r"#\[\s*(?:tokio::)?test\b")
# Any attribute line #[...] (stacks above a fn without clearing the pending
# test flag — #[should_panic], #[ignore], #[cfg(...)] may sit between the
# test attr and the fn).
_ATTR_LINE_RE = re.compile(r"^\s*#\[")
_FN_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)\s*[(<]")
_TYPE_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|trait|union)\s+([A-Za-z0-9_]+)\b"
)
# Project-specific bug/reference prefixes in test names (bug1_, bug2_, ...).
_BUG_PREFIX_RE = re.compile(r"^bug\d+(_|$)")
# Function words that cannot end a name: they take a complement, so a name
# ending in one is a severed phrase, not a name. A test called
# test_mandatory_deny_read_and is missing its object; test_inner_half_open_after
# is missing what it comes after. These arise from trimming a too-long name by
# dropping trailing segments, which satisfies the underscore cap while
# destroying the sentence -- the cap counts separators and cannot see that the
# clause no longer closes. Renaming for the behavior is the fix; cutting the
# last word is not.
_DANGLING_TAIL_WORDS = frozenset(
    {
        "a", "an", "the", "and", "or", "but", "not", "no", "as", "than", "that",
        "if", "when", "then", "after", "before", "with", "without", "from",
        "to", "into", "of", "on", "at", "for", "in", "by", "is", "are", "was",
        "were", "be", "been", "has", "have", "do", "does", "can", "must",
        "should", "will", "it", "its",
    }
)

_TEST_NAME_LEN_SOFT = 40
_FN_LEN_WARN = 50
_TYPE_LEN_WARN = 40

# Pre-existing flat-prefix module pairs (rule 8), as the CHILD path relative
# to the repo root. The gate tolerates these so it can land before the
# directory-module migration; a flat prefix not listed here is a NEW one and
# fails the gate. Delete a line in the same commit that converts the pair to
# X.rs + X/<suffix>.rs, so the baseline ratchets toward empty. When this set
# is empty the rule is fully enforced.
_FLAT_PREFIX_BASELINE: frozenset[str] = frozenset({
})

# File-name blacklist: meaningless qualifiers (extra/partN/misc) and the
# non-standard test-naming forms (tests_ prefix, _tests_ infix) that
# is_test_file no longer recognizes. Legit _tests.rs / tests.rs /
# test_support.rs do not match.
_FILE_QUALIFIER_RE = re.compile(r"(?:_extra|_part\d|_misc)[_.]|^tests_|_tests_")
_FILE_QUALIFIER_BASELINE: frozenset[str] = frozenset({
})


def _check_file_qualifiers(errors: list[str], root: Path = Path("crates")) -> None:
    """Flag .rs files whose name carries a meaningless qualifier (extra /
    partN / misc). Pre-existing ones are grandfathered in the baseline; a
    new one fails. A baseline entry whose file no longer exists is a stale
    line (same ratchet honesty as the flat-prefix baseline).

    Scans the repo tree (crates/), not the args list — a caller passing a
    single file will not see this check fire on it."""
    detected: set[str] = set()
    for p in root.rglob("*.rs"):
        if "/target/" in str(p):
            continue
        rel = p.as_posix()
        if _FILE_QUALIFIER_RE.search(p.name):
            detected.add(rel)
            if rel not in _FILE_QUALIFIER_BASELINE:
                errors.append(
                    f"{rel}: file name has a meaningless qualifier "
                    f"({_FILE_QUALIFIER_RE.search(p.name).group(0)}); "
                    f"name the behavior domain, do not call it extra/partN/misc"
                )
    stale = _FILE_QUALIFIER_BASELINE - detected
    for s in sorted(stale):
        errors.append(
            f"{s}: stale _FILE_QUALIFIER_BASELINE entry -- file no longer "
            f"on disk; remove this line"
        )


# Naming-convention baselines: get_ prefix (Java-accessor style; Rust uses the
# field name or a bare fn, not get_foo) and vague type suffixes (Info/Data/
# Wrapper/Manager/Helper/Util/Base signal a grab-bag struct, not a named
# domain). Both grandfather the existing backlog so the gate lands before
# the cleanup; a new occurrence fails. handle_ is NOT get_ and is never
# flagged.
#
# get_or_create_* is exempt — it is a compound get-or-initialize operation
# (a read with a side effect: create if absent), not a pure value accessor.
# The get_ ban targets pure accessors (Rust idiom: name the field, do not
# prefix with get_); get_or_create has no field-name form, so the ban does
# not apply.
_GET_PREFIX_EXEMPT = ("get_or_create_",)
_GET_PREFIX_BASELINE: frozenset[str] = frozenset({
    "crates/houyicoder-tui/src/selection.rs:get_clipboard_path",
})
_VAGUE_SUFFIX_RE = re.compile(r"(?:Info|Data|Wrapper|Manager|Helper|Util|Base)$")
_VAGUE_SUFFIX_BASELINE: frozenset[str] = frozenset({
    "crates/houyicoder-cli/src/export_bridge.rs:ExportData",
    "crates/houyicoder-core/src/agent/compact_hook_tests.rs:CapturingSummarizerWrapper",
    "crates/houyicoder-permission/src/pipeline/mod.rs:ValidatorInfo",
    "crates/houyicoder-tui/src/evidence.rs:DiffData",
    "crates/houyicoder-tui/src/git_op.rs:PrInfo",
})


def _check_naming(errors: list[str], root: Path = Path("crates")) -> None:
    """Flag new get_ prefix fns and vague-suffix (Info/Data/Wrapper/Manager/
    Helper/Util/Base) types. State is deliberately excluded -- in Rust it is
    often legitimate (UI state machines, toggle state); the 7 in-repo
    State-suffixed structs are all legit UI/state, so adding it would be
    pure false-positive churn.
    Existing backlog is grandfathered in the baselines; new occurrences fail.
    Stale baseline entries (item removed) are errors so the ratchet stays
    honest — same shape as the flat-prefix + file-qualifier baselines.

    Scans the repo tree (crates/), not the args list — unlike the fn/type
    name checks above which scan $RS_FILES. This is correct for make check
    (where $RS_FILES is git ls-files) but a caller passing a single file
    will not see these three checks fire on it."""
    get_hits: set[str] = set()
    vague_hits: set[str] = set()
    for p in root.rglob("*.rs"):
        if "/target/" in str(p):
            continue
        rel = p.as_posix()
        try:
            text = p.read_text(encoding="utf-8")
        except OSError:
            continue
        for line in text.splitlines():
            m = _FN_RE.match(line)
            if m:
                name = m.group(1)
                if name.startswith("get_") and not name.startswith(_GET_PREFIX_EXEMPT):
                    get_hits.add(f"{rel}:{name}")
            m = _TYPE_RE.match(line)
            if m and _VAGUE_SUFFIX_RE.search(m.group(1)):
                vague_hits.add(f"{rel}:{m.group(1)}")
    for hit in sorted(get_hits):
        if hit not in _GET_PREFIX_BASELINE:
            name = hit.split(":", 1)[1] if ":" in hit else hit
            errors.append(
                f"{hit}: fn '{name}' uses get_ prefix; Rust uses the field "
                f"name or a bare fn, not a Java accessor"
            )
    for hit in sorted(vague_hits):
        if hit not in _VAGUE_SUFFIX_BASELINE:
            errors.append(
                f"{hit}: type ends in a vague suffix (Info/Data/Wrapper); "
                f"name the domain, do not call it Info/Data/Wrapper"
            )
    for s in sorted(_GET_PREFIX_BASELINE - get_hits):
        errors.append(f"{s}: stale get_ baseline entry -- fn no longer on disk")
    for s in sorted(_VAGUE_SUFFIX_BASELINE - vague_hits):
        errors.append(f"{s}: stale vague-suffix baseline entry -- type no longer on disk")


def _flat_prefix_pairs(src_dirs: list[Path]) -> list[tuple[Path, str, str]]:
    """Return [(dir, parent_stem, child_stem)] for every flat-prefix pair
    under the given src directories. A pair is X.rs + Y.rs where Y starts with
    X_ and X is the longest existing parent stem (so a name whose own stem
    contains underscores pairs with its real parent, not a shorter one).
    Excludes any child ending in _tests (peer-test convention) and any parent
    ending in _tests (a test-file family, not a source hierarchy)."""
    pairs: list[tuple[Path, str, str]] = []
    seen_dirs: set[Path] = set()
    for d in src_dirs:
        if d in seen_dirs:
            continue
        seen_dirs.add(d)
        stems = sorted(f.stem for f in d.glob("*.rs"))
        stemset = set(stems)
        for child in stems:
            if child.endswith("_tests"):
                continue
            parts = child.split("_")
            best = None
            for i in range(1, len(parts)):
                cand = "_".join(parts[:i])
                if cand in stemset and cand != child and not cand.endswith("_tests"):
                    best = cand  # keep extending -- longest existing parent wins
            if best:
                pairs.append((d, best, child))
    return pairs


def _check_flat_prefix(
    errors: list[str],
    root: Path = Path("crates"),
    baseline: "frozenset[str]" = _FLAT_PREFIX_BASELINE,
) -> None:
    src_dirs = [p.parent for p in root.rglob("*.rs")
                if "/src/" in str(p) and "/tests/" not in str(p)]
    detected: set[str] = set()
    for d, parent, child in _flat_prefix_pairs(src_dirs):
        child_rel = (d / f"{child}.rs").as_posix()
        detected.add(child_rel)
        if child_rel not in baseline:
            errors.append(
                f"{child_rel}: flat-prefix module pair ({parent}.rs + "
                f"{child}.rs); use a directory module {parent}/{child[len(parent) + 1:]}.rs "
                f"so the path is {parent}::{child[len(parent) + 1:]}, not a "
                f"prefix simulation. If this is pre-existing, add it to "
                f"_FLAT_PREFIX_BASELINE; otherwise convert to a directory module."
            )
    # Staleness: a baseline path whose pair is no longer on disk (converted to
    # a directory module, or deleted) is a dead line. tolerated prints but
    # never reconciles, so the ratchet would silently fail -- a stale line is
    # an ERROR so "converting a pair drops its line" is gate-enforced, not a
    # comment convention. Same shape as the stale-blank lesson in the
    # acceptance docs: an untracked record is worse than none.
    stale = baseline - detected
    for s in sorted(stale):
        errors.append(
            f"{s}: stale _FLAT_PREFIX_BASELINE entry -- the pair is no longer "
            f"on disk (converted or deleted). Remove this line in the same "
            f"commit that converted the pair, so the ratchet stays honest."
        )
    # Live baseline entries (backed by a pair still on disk) print so the
    # stock is visible; stale ones are errors above, not silently tolerated.
    live = baseline & detected
    if live:
        print(
            f"[flat-prefix] {len(live)} pre-existing pair(s) tolerated "
            f"(see _FLAT_PREFIX_BASELINE); ratchet down as pairs convert.",
            file=sys.stderr,
        )


def _jargon_violation(path, lineno, name, kind, errors) -> None:
    """Flag ad-hoc jargon (mid-run) in any identifier name. The ban extends
    from test names (hook_rust, write-time) to all fn/type names here at L2
    — 'mid-run' is not standard vocabulary; use during_run / in_flight /
    concurrent (AGENTS.md Naming)."""
    jm = JARGON.search(name)
    if jm:
        errors.append(
            f"{path}:{lineno}: {kind} name '{name}' contains jargon "
            f"'{jm.group(0)}'; use program-design vocabulary (during_run, "
            f"in_flight, concurrent) instead of ad-hoc terms (AGENTS.md)"
        )


def _check_test_fn(
    path: Path, lineno: int, name: str, errors: list[str], warns: list[str]
) -> None:
    if not name.startswith("test_"):
        errors.append(
            f"{path}:{lineno}: test fn '{name}' lacks the test_ prefix; "
            f"use test_<subject>_<behavior> (#[test] marks it to rustc, the "
            f"prefix marks it to grep + test-output readers)"
        )
    if _BUG_PREFIX_RE.match(name):
        errors.append(
            f"{path}:{lineno}: test name '{name}' starts with a "
            f"project-specific bugN_ prefix; describe the behavior instead"
        )
    tail = name.rsplit("_", 1)[-1]
    if tail in _DANGLING_TAIL_WORDS:
        msg = (
            f"{path}:{lineno}: test name '{name}' ends on the function word "
            f"'{tail}', so the phrase never closes -- a severed name, almost "
            f"always from trimming by dropping trailing segments. Rename for "
            f"the behavior instead of cutting the last word"
        )
        errors.append(msg)
    underscores = name.count("_")
    if underscores > TEST_UNDERSCORE_CAP:
        errors.append(
            f"{path}:{lineno}: test name '{name}' has {underscores} underscores "
            f"(>{TEST_UNDERSCORE_CAP}); trim or move detail into a module doc"
        )
    if len(name) > TEST_NAME_LEN_HARD:
        errors.append(
            f"{path}:{lineno}: test name '{name}' is {len(name)} chars "
            f"(>{TEST_NAME_LEN_HARD} hard limit)"
        )
    elif len(name) > _TEST_NAME_LEN_SOFT:
        warns.append(
            f"{path}:{lineno}: test name '{name}' is {len(name)} chars "
            f"(>{_TEST_NAME_LEN_SOFT}, consider trimming)"
        )
    _jargon_violation(path, lineno, name, "test", errors)


def _scan(path: Path, errors: list[str], warns: list[str]) -> None:
    if not path.is_file():
        return
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return
    # True when the next fn is a test fn (a test attribute was seen above,
    # possibly with other attrs / blank / doc lines in between).
    pending_test = False
    for lineno, line in enumerate(text.splitlines(), start=1):
        if _ATTR_LINE_RE.match(line):
            if _TEST_ATTR_RE.search(line):
                pending_test = True
            # Other attrs stack; they neither set nor clear the pending flag.
            continue
        m = _FN_RE.match(line)
        if m:
            name = m.group(1)
            is_test = pending_test
            pending_test = False
            if is_test:
                _check_test_fn(path, lineno, name, errors, warns)
            else:
                _jargon_violation(path, lineno, name, "fn", errors)
                if len(name) > _FN_LEN_WARN:
                    warns.append(
                        f"{path}:{lineno}: fn name '{name}' is {len(name)} chars "
                        f"(>{_FN_LEN_WARN}, consider trimming)"
                    )
            continue
        m = _TYPE_RE.match(line)
        if m:
            name = m.group(1)
            _jargon_violation(path, lineno, name, "type", errors)
            if name.endswith("Wire"):
                errors.append(
                    f"{path}:{lineno}: type name '{name}' ends in 'Wire'; wire "
                    f"DTOs share a domain name and disambiguate by path, or use "
                    f"a descriptive name"
                )
            if len(name) > _TYPE_LEN_WARN:
                warns.append(
                    f"{path}:{lineno}: type name '{name}' is {len(name)} chars "
                    f"(>{_TYPE_LEN_WARN}, consider trimming)"
                )
            pending_test = False
            continue
        # A non-attr, non-blank, non-comment line ends any pending test attr
        # (the attr must sit directly above the fn; code in between means the
        # attr was not for a fn on the next line). Blank lines and //
        # comments do not clear it.
        stripped = line.strip()
        if stripped and not stripped.startswith("//"):
            pending_test = False


def main(paths: list[str]) -> int:
    files = [Path(p) for p in paths if p.endswith(".rs")] or list(
        Path("crates").rglob("*.rs")
    )
    errors: list[str] = []
    warns: list[str] = []
    for p in files:
        _scan(p, errors, warns)
    _check_flat_prefix(errors)
    _check_file_qualifiers(errors)
    _check_naming(errors)
    for w in warns:
        print(f"warn: {w}", file=sys.stderr)
    for e in errors:
        print(f"error: {e}", file=sys.stderr)
    if errors:
        print(f"\n[Naming] {len(errors)} error(s) in .rs names.", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
