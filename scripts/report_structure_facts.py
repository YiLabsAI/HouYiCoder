#!/usr/bin/env python3
"""Structure facts report for the deep-review standard.

Report-only (exit 0 always). Wired into make verify so the facts print
before "verify passed". The deep-review standard requires countable
facts to be surfaced by tooling so review spends budget on judgment,
not data-gathering: if the reviewer must manually count "how many files
match this enum", the review is not done.

Detectors (all grep/regex-based, read-only):
  1. Suppression inventory: allow/expect count + reason extraction
  2. Enum match-site count: wire enums matched across N files
  3. File line-count band: 500-800 refactor zone, >=800 error zone
  4. Struct field count: >FIELD_WARN_THRESHOLD warn, >FIELD_REVIEW_THRESHOLD review
  5. Cross-file clone ratchet: jscpd duplicated lines vs CLONE_BASELINE
  6. New-word introductions: identifiers added vs the dev merge-base whose
     words are not in the repo's vocabulary. The codebase IS the glossary
     (no separate word list to drift); a new word is a signal for review
     (new concept vs synonym), not a verdict.
  7. Module dead_code ratchet: #![allow(dead_code)] count + file list vs
     MODULE_DEAD_CODE_BASELINE. These suppress dead_code for ENTIRE files --
     #[expect] cannot replace them (cfg-sensitive). The ratchet blocks new
     ones; the L3 report lists the files so deep review asks "still needed?"
     when a file is touched.

Trigger conditions (any hit -> print the review checklist so the
reviewer spends 10 min on known facts, not 30 min digging):
  - wire enum matched >=3 files (projection duplication candidate)
  - files >=800 lines (size-gate error zone)
  - structs >FIELD_REVIEW_THRESHOLD fields (god-struct candidate)
  - clone delta >0 (vs CLONE_BASELINE)
  - suppression without-reason >0

Run: python3 scripts/report_structure_facts.py  (always exit 0)
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.paths import TEST_IGNORE_GLOBS, is_test_file, rs_source_files  # noqa: E402

REPO = Path(__file__).resolve().parent.parent

# Wire enums whose match-site count signals projection duplication. A new
# variant added to one of these must surface in every match site or it
# silently drops — the projection-duplication concern.
WIRE_ENUMS = [
    "TurnEventKind",
    "FrontendRequest",
    "AgentMessage",
    "ToolCallStatus",
    "PermissionMode",
    "SessionUpdate",
]

# Exclusion policy (test files):
#   - enum_match_sites + clone_ratchet: EXCLUDE tests. The signal is
#     production duplication — matching an enum in a test is normal, not a
#     projection smell. Counting test files here would drown the signal.
#   - file_line_bands + struct_field_counts + suppression: KEEP tests. A
#     800-line test file or a 20-field test struct is still backlog; a test
#     #[allow] is still a suppression that should carry a reason.
# The shared predicate is rules/paths.py::is_test_file so the two excluders
# cannot drift apart (the repo has six test-naming conventions; matching
# only one or two silently corrupts the count — the naming cost made
# tangible).

# Struct field-count thresholds. warn = review the split; review = a
# god-struct candidate. Numbers live here only — the docstring + output
# reference these names so the threshold has one source of truth.
FIELD_WARN_THRESHOLD = 8
FIELD_REVIEW_THRESHOLD = 12

# jscpd cross-file production-clone lines, measured with TEST_IGNORE_GLOBS
# (the same ignore string the script uses, so current and baseline are
# comparable). Re-derive by running the script once on a clean tree.
CLONE_BASELINE = 2808

# Module-level #![allow(dead_code)] suppressions. These turn off dead_code
# for an ENTIRE file -- bigger than per-item allow (one fn). #[expect]
# cannot replace them: dead_code is cfg-sensitive (code alive in one
# target, dead in another), so #[expect(dead_code)] is unfulfilled in the
# target where the code is alive -> error. The ratchet baseline blocks NEW
# module-level dead_code suppressions; the L3 report lists the files so
# deep review asks "still needed?" when a file is touched.
MODULE_DEAD_CODE_RE = re.compile(r"#!\[allow\(dead_code\)\]")
MODULE_DEAD_CODE_BASELINE = 16  # measured 2026-08-17 (14 production + 2
# test helpers in tests/common/mod.rs; test modules' dead helpers are
# legit but the suppression should still be visible)


def _rg(pattern: str, *args: str) -> str:
    """Run ripgrep, return stdout (empty if no hits / not installed)."""
    try:
        r = subprocess.run(
            ["rg", pattern, *args, "--type", "rust", "--no-heading", "-N"],
            capture_output=True,
            text=True,
            cwd=str(REPO),
        )
        return r.stdout
    except (OSError, subprocess.CalledProcessError):
        return ""


def suppression_inventory() -> dict:
    """Count allow/expect attributes + extract reasons."""
    allow = 0
    expect = 0
    reasons: dict[str, int] = {}
    without_reason = 0
    for f in rs_source_files(REPO):
        try:
            text = f.read_text(encoding="utf-8")
        except OSError:
            continue
        for m in re.finditer(r"#\[(allow|expect)\(([^)]*)\)", text):
            kind = m.group(1)
            body = m.group(2)
            if kind == "allow":
                allow += 1
            else:
                expect += 1
            # Extract reason if present.
            rm = re.search(r'reason\s*=\s*"([^"]*)"', body)
            if rm:
                reasons[rm.group(1)] = reasons.get(rm.group(1), 0) + 1
            else:
                without_reason += 1
    return {
        "allow": allow,
        "expect": expect,
        "with_reason": sum(reasons.values()),
        "without_reason": without_reason,
        "reasons": reasons,
    }


def enum_match_sites() -> dict[str, list[str]]:
    """For each wire enum, list files where its variants appear in match
    branches. A line like an enum variant followed by an arrow is a match
    branch; >=3 files = projection-duplication candidate.
    """
    sites: dict[str, list[str]] = {}
    for enum in WIRE_ENUMS:
        # Match branches: enum variant ... =>. Much narrower than all references.
        out = _rg(rf"\b{enum}::[A-Za-z_]+.*=>")
        files = sorted({line.split(":")[0] for line in out.splitlines() if line})
        rel = []
        for f in files:
            try:
                rp = str(Path(f).relative_to(REPO))
            except ValueError:
                rp = f
            if not is_test_file(rp):  # exclude tests — production signal only
                rel.append(rp)
        sites[enum] = rel
    return sites


def file_line_bands() -> dict:
    """Count .rs files by line-count band (refactor signal)."""
    lt500 = 0
    band_500_800 = 0
    ge800 = 0
    ge800_files = []
    for f in rs_source_files(REPO):
        try:
            n = sum(1 for _ in f.open(encoding="utf-8"))
        except OSError:
            continue
        if n >= 800:
            ge800 += 1
            ge800_files.append((str(f.relative_to(REPO)), n))
        elif n >= 500:
            band_500_800 += 1
        else:
            lt500 += 1
    ge800_files.sort(key=lambda x: -x[1])
    return {
        "lt500": lt500,
        "band_500_800": band_500_800,
        "ge800": ge800,
        "ge800_files": ge800_files,
    }


def struct_field_counts() -> list[tuple[str, int]]:
    """Count fields per struct. Line-based brace-matching that skips
    braces inside line comments (a doc comment with a brace would otherwise
    inflate depth and overshoot past the struct's real closing brace)."""
    results = []
    struct_re = re.compile(r"^\s*(?:pub\s+)?struct\s+([A-Z]\w+)\s*\{")
    field_re = re.compile(r"^\s*(?:pub\s+)?\w+\s*:")
    for f in rs_source_files(REPO):
        try:
            lines = f.read_text(encoding="utf-8").splitlines()
        except OSError:
            continue
        for i, line in enumerate(lines):
            m = struct_re.match(line)
            if not m:
                continue
            name = m.group(1)
            depth = 1  # the { on the struct line
            fields = 0
            j = i + 1
            while j < len(lines) and depth > 0:
                # Strip line comments so braces inside doc prose don't skew depth.
                code_part = lines[j].split("//", 1)[0]
                depth += code_part.count("{") - code_part.count("}")
                if depth > 0:
                    stripped = lines[j].strip()
                    if field_re.match(stripped) and not stripped.startswith("//"):
                        fields += 1
                j += 1
            if fields > FIELD_WARN_THRESHOLD:
                rel = str(f.relative_to(REPO))
                results.append((f"{rel}:{name}", fields))
    results.sort(key=lambda x: -x[1])
    return results


def clone_ratchet() -> dict:
    """Run jscpd over production .rs (test files excluded via the shared
    TEST_IGNORE_GLOBS so production-only clones are counted), extract total
    duplicated lines vs baseline. jscpd is version-locked (5.0.15 via
    scripts/ensure_jscpd.sh) so the count is comparable across machines +
    CI; an unlocked version drifts the count and makes the baseline
    meaningless. Report-only (a trend, not a blocking ratchet) -- jscpd's
    count is noisy and the tool itself has no stable JSON mode."""
    try:
        r = subprocess.run(
            ["jscpd", "--silent", "--format", "rust", "--ignore", TEST_IGNORE_GLOBS,
             str(REPO / "crates")],
            capture_output=True,
            text=True,
            cwd=str(REPO),
            timeout=60,
        )
        # jscpd exits 0 (no clones) or 1 (clones found); both carry the
        # summary line on stdout. Parse the duplicated-lines count from text
        # (jscpd has no stable JSON-to-stdout mode; the summary line is stable).
        out = r.stdout
        m = re.search(r"(\d+(?:\.\d+)?)\(", out)  # "3747(3.52%)" -> 3747
        if not m:
            return {"current": None, "baseline": CLONE_BASELINE, "delta": None}
        current = int(float(m.group(1)))
        return {
            "current": current,
            "baseline": CLONE_BASELINE,
            "delta": current - CLONE_BASELINE,
        }
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return {"current": None, "baseline": CLONE_BASELINE, "delta": None}


def module_dead_code_ratchet() -> dict:
    """Count module-level #![allow(dead_code)] + list the files. These
    suppress dead_code for an ENTIRE file -- bigger than per-item allow.
    #[expect] cannot replace them (dead_code is cfg-sensitive: alive in
    one target, dead in another -> expect unfulfilled). The ratchet
    baseline blocks new ones; the L3 report lists files so deep review
    asks 'still needed?' when a file is touched."""
    hits = []
    for f in rs_source_files(REPO):
        try:
            text = f.read_text(encoding="utf-8")
        except OSError:
            continue
        if MODULE_DEAD_CODE_RE.search(text):
            hits.append(str(f.relative_to(REPO)))
    return {
        "current": len(hits),
        "baseline": MODULE_DEAD_CODE_BASELINE,
        "delta": len(hits) - MODULE_DEAD_CODE_BASELINE,
        "files": hits,
    }


# Declaration keywords that introduce a NEW name. impl is excluded: an impl
# block names an existing trait/type, it does not declare a new one. let is
# excluded as too noisy (locals).
_DECL_RE = re.compile(
    r"\b(?:fn|struct|enum|trait|const|static|type|mod)\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def _identifier_words(name: str) -> list[str]:
    """Split a snake_case / camelCase identifier into lowercase words of len>=2
    with at least one alpha (keeps base64, drops i / x / 1)."""
    raw = name.replace("_", " ")
    spaced = re.sub(r"([a-z])([A-Z])", r"\1 \2", raw)
    return [
        w.lower()
        for w in spaced.split()
        if len(w) >= 2 and any(c.isalpha() for c in w)
    ]


def _declared_names(text: str) -> list[str]:
    return [m.group(1) for m in _DECL_RE.finditer(text)]


def new_word_introductions() -> dict:
    """Identifiers introduced by the current change (working tree vs the dev
    merge-base) whose words are NOT in the repo's existing vocabulary.

    The codebase IS the glossary -- no separate word list to maintain, so it
    cannot drift (a stored glossary is a second copy of knowledge that always
    falls behind). A "new word" is a signal, not a verdict: review asks
    "new concept (accept) or synonym of an existing word (use the existing
    one)?" -- dotdot vs traversal is the canonical case. Only flags INCREMENT
    naming; the 8 existing synonym families (fence/containment/boundary ...)
    are backlogged design decisions for L4, out of scope here.

    impl + let are excluded (impl names an existing trait; let locals are
    noise). Words of len < 2 or pure-numeric are dropped.
    """
    try:
        base = subprocess.run(
            ["git", "merge-base", "HEAD", "dev"],
            capture_output=True, text=True, cwd=str(REPO),
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        base = ""
    if not base:
        base = "HEAD"
    # Vocabulary: words in declarations at the base tree (the codebase before
    # this change). git grep over the base ref + the crates pathspec.
    try:
        head_grep = subprocess.run(
            ["git", "grep", "-h", "-I", "-P", _DECL_RE.pattern, base, "--", "crates"],
            capture_output=True, text=True, cwd=str(REPO), timeout=60,
        ).stdout
    except (OSError, subprocess.SubprocessError, subprocess.TimeoutExpired):
        head_grep = ""
    vocab: set[str] = set()
    for name in _declared_names(head_grep):
        vocab.update(_identifier_words(name))
    # git grep -P can be unavailable (some builds lack PCRE: non-zero exit,
    # empty stdout, no exception). Without this guard, vocab=0 flags every
    # added identifier as "new" -- a report-only detector crying wolf gets
    # ignored. Surface the failure; empty input is not an empty result.
    if len(vocab) < 100:
        return {
            "base": base[:8],
            "vocab": len(vocab),
            "new": {},
            "error": "vocabulary empty or tiny -- git grep -P unavailable "
                     "or the base ref has no .rs declarations",
        }
    # Added declarations in the working-tree diff vs base.
    try:
        diff = subprocess.run(
            ["git", "diff", base, "--", "crates"],
            capture_output=True, text=True, cwd=str(REPO), timeout=60,
        ).stdout
    except (OSError, subprocess.SubprocessError, subprocess.TimeoutExpired):
        diff = ""
    added: list[str] = []
    for line in diff.splitlines():
        if line.startswith("+") and not line.startswith("+++"):
            added.extend(_declared_names(line))
    # Preserve first-seen order + dedup.
    seen: set[str] = set()
    ordered_added: list[str] = []
    for name in added:
        if name not in seen:
            seen.add(name)
            ordered_added.append(name)
    new: dict[str, list[str]] = {}
    for name in ordered_added:
        new_words = [w for w in _identifier_words(name) if w not in vocab]
        if new_words:
            new[name] = new_words
    return {"base": base[:8], "vocab": len(vocab), "new": new}


def main() -> int:
    sup = suppression_inventory()
    enums = enum_match_sites()
    bands = file_line_bands()
    structs = struct_field_counts()
    clones = clone_ratchet()
    new_words = new_word_introductions()
    mod_dead = module_dead_code_ratchet()

    print("=== Structure Facts (report-only) ===")
    print()

    # 1. Suppression inventory
    print("[Suppression]")
    print(f"  allow={sup['allow']} expect={sup['expect']} "
          f"with-reason={sup['with_reason']} without-reason={sup['without_reason']}")
    if sup["reasons"]:
        reason_list = ", ".join(f"{r} x{n}" for r, n in sorted(sup["reasons"].items(), key=lambda x: -x[1]))
        print(f"  expect reasons: {reason_list}")
    print()

    # 2. Enum match sites
    print("[Enum Match Sites] (>=3 files = projection-duplication candidate)")
    for enum, files in enums.items():
        marker = " <== DUP" if len(files) >= 3 else ""
        print(f"  {enum}: {len(files)} files{marker}")
    print()

    # 3. File line-count band
    print("[File Line Count]")
    print(f"  <500: {bands['lt500']} | 500-800: {bands['band_500_800']} | >=800: {bands['ge800']}")
    for path, n in bands["ge800_files"][:5]:
        print(f"    >=800: {path} ({n})")
    print()

    # 4. Struct field count
    print(f"[Struct Fields] (>{FIELD_WARN_THRESHOLD} warn, >{FIELD_REVIEW_THRESHOLD} review)")
    for path_name, n in structs[:8]:
        marker = " REVIEW" if n > FIELD_REVIEW_THRESHOLD else " warn"
        print(f"  {n} fields{marker}: {path_name}")
    if not structs:
        print(f"  (none >{FIELD_WARN_THRESHOLD})")
    print()

    # 5. Clone ratchet
    print("[Cross-File Clones]")
    if clones["current"] is not None:
        delta = clones["delta"]
        delta_str = f" (+{delta})" if delta and delta > 0 else (f" ({delta})" if delta else "")
        print(f"  baseline={clones['baseline']} current={clones['current']}{delta_str}")
    else:
        print(f"  baseline={clones['baseline']} (jscpd unavailable)")
    print()

    # 6. New-word introductions (naming drift signal)
    print(f"[New Words] (vocab {new_words['vocab']} at base {new_words['base']})")
    if new_words.get("error"):
        print(f"  ERROR: {new_words['error']} -- detector unreliable, do not trust (none) below")
    elif new_words["new"]:
        for name, words in new_words["new"].items():
            print(f"  {name}: new word(s) {', '.join(words)}")
    else:
        print("  (none -- every introduced identifier's words are in the repo vocab)")
    print()

    # 7. Module-level dead_code suppressions (blind-spot ratchet)
    print("[Module dead_code] (#![allow(dead_code)] -- suppresses ENTIRE file)")
    print(f"  baseline={mod_dead['baseline']} current={mod_dead['current']}"
          f"{' +' + str(mod_dead['delta']) if mod_dead['delta'] > 0 else ''}")
    for f in mod_dead["files"][:8]:
        print(f"  {f}")
    print()

    # Trigger conditions + review checklist
    triggers = []
    for enum, files in enums.items():
        if len(files) >= 3:
            triggers.append(f"enum matched >=3 files: {enum} ({len(files)})")
    if bands["ge800"] > 0:
        triggers.append(f"files >=800: {bands['ge800']}")
    god_structs = [s for s in structs if s[1] > FIELD_REVIEW_THRESHOLD]
    if god_structs:
        triggers.append(f"structs >{FIELD_REVIEW_THRESHOLD} fields: {len(god_structs)}")
    if clones["delta"] is not None and clones["delta"] > 0:
        triggers.append(f"clone increase: +{clones['delta']}")
    if sup["without_reason"] > 0:
        triggers.append(f"suppression without-reason: {sup['without_reason']}")
    if new_words["new"]:
        triggers.append(f"new naming words: {len(new_words['new'])}")
    if mod_dead["delta"] > 0:
        triggers.append(f"module dead_code increase: +{mod_dead['delta']}")

    print("=== Triggers ===")
    if triggers:
        for t in triggers:
            print(f"  [HIT] {t}")
        print()
        print("=== Deep Review Checklist ===")
        print("1. Suppression direction: check diff for allow/expect net change")
        print("2. Abstraction: new trait/helper? call-site count >=2?")
        print("3. Cross-file duplication: see enum match sites + clone ratchet above")
        print("4. Reason truthfulness: see expect reasons above; verify per-function")
        print(f"5. Struct field count: see struct fields above; flag >{FIELD_WARN_THRESHOLD}/>{FIELD_REVIEW_THRESHOLD}")
        print("6. Naming: see new words above; each is a new concept or a synonym of an existing word?")
        print("7. Module dead_code: see files above; if a touched file has #![allow(dead_code)], is it still needed?")
    else:
        print("  (no triggers — deep review optional for this change)")

    return 0


if __name__ == "__main__":
    sys.exit(main())
