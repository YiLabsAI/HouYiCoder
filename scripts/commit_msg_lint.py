#!/usr/bin/env python3
"""Commit-msg lint. Reuses the SAME rules the PreToolUse hook applies to
.rs comments (rules/comments.py, shared with hook_rust.py), so write-time
and commit-time stay consistent and complementary -- one word source, two
enforcement points. Memory fades across sessions; this gate is the machine
backstop so the rules do not depend on the writer remembering them.

Reused from rules.comments (no reinvention):
  - CODENAME: internal task/sprint/gate tags (letter+digit sprint/task ids,
    stage refs, section refs, bet forms). Applied to subject + body.

Complementary (commit-specific, not in the .rs gate):
  - letter+digit phase-gate / journey / hazard / priority ids and dotted
    version stamps (in _ACCEPTANCE_CODES): internal-doc indices a git-log
    reader cannot decode. Design docs (.md) are never scanned, so they keep
    carrying them legitimately.
  - ASCII-only subject: commit messages are English imperative (AGENTS.md).
    CJK in the subject trips here; the .rs gate trips CJK in comments.

NOT applied here (intentional complement, not an oversight):
  - OWN_NAME (houyicoder/hicoder, in rules.comments but not imported here):
    the .rs gate bans it because comments describe concerns, not crates. A
    commit subject legitimately names the crate it modifies
    (feat(ports): scaffold houyicoder-api), so banning own-name here would
    be wrong.

Reads the commit message from a file path arg (git commit-msg hook passes
the temp file). Exit 0 = ok, exit 1 = block (stderr shown to the author).
"""
import re
import sys
from pathlib import Path

# Reuse the shared comment-rule patterns (rules/comments.py), not a parallel
# wordlist. write-time hook (hook_rust.py) + check-time gate + commit-time all
# share one source.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.comments import CODENAME, COMPARISON, product_pattern  # noqa: E402

# Acceptance/charter internal tracking codes the shared CODENAME misses:
# letter+digit phase-gate / journey / hazard ids and dotted version stamps.
# They are internal-doc indices an outside git-log reader cannot decode, so
# they are banned from commit prose. Kept commit-only (out of the .rs
# comment hook) because the same letter+digit shape appears in legitimate
# .rs prose (headings, version strings); P moved into the shared CODENAME
# since the codebase carries no such literal there.
_ACCEPTANCE_CODES = re.compile(r"\b[GUH]\d+\b|\bv\d+\.\d+\b")

# Phase/sprint/gate/milestone/stage codes written as full words + space +
# number ("Phase 2", "Stage 2", etc.). These slip past _ACCEPTANCE_CODES
# (which expects letter+digit adjacent, not a word + space + number) and
# past CODENAME (which catches the space-less form but not "Stage 2"). An
# outside git-log reader cannot decode "Phase 2" any more than the
# letter+digit form -- ban both. re.IGNORECASE so lowercase is caught too.
# "Step N" is NOT included (too common in real commit instructions like
# "step 1: install").
_PHASE_WORDS = re.compile(
    r"\b(?:Phase|Sprint|Gate|Milestone|Stage)\s+\d+\b", re.IGNORECASE
)


# Scissors line: git commit -v appends a diff below a separator line.
# The real line is "# ------------------------ >8 ------------------------"
# (the commentChar prefix may differ if the user configured one), so match
# by shape, not by literal. Without stripping, the lint scans the diff
# itself - and a commit that erases a name from a .rs comment would trip
# its own removed line.
_SCISSORS = re.compile(r"^\s*\S?\s*-{4,}\s*>8\s*-{4,}", re.MULTILINE)


def _lint_lines(msg: str) -> list[tuple[int, str]]:
    """Return (original_line_number, text) for lines the lint should scan:
    everything up to the scissors line, skipping git-template # lines
    (which carry git's own instructions, not the author's prose). Line
    numbers are the original file's so error messages point at the right
    line the reader can find."""
    m = _SCISSORS.search(msg)
    if m:
        msg = msg[: m.start()]
    out = []
    for i, line in enumerate(msg.splitlines(), start=1):
        if line.lstrip().startswith("#"):
            continue
        out.append((i, line))
    return out


def main() -> int:
    if len(sys.argv) < 2:
        print("commit_msg_lint: usage: commit_msg_lint.py <msg_file>", file=sys.stderr)
        return 1
    msg_path = Path(sys.argv[1])
    msg = msg_path.read_text(encoding="utf-8")
    lines = _lint_lines(msg)
    if not lines:
        return 0
    subject = lines[0][1]

    # 1. ASCII-only subject (English imperative, per AGENTS.md).
    non_ascii = [c for c in subject if ord(c) > 0x7F]
    if non_ascii:
        sample = "".join(non_ascii[:8])
        print(
            f"ERROR: commit subject must be ASCII (English, imperative). "
            f"Found non-ASCII {sample!r} in: {subject}",
            file=sys.stderr,
        )
        return 1

    # 2. Internal codenames: task/sprint tags + acceptance/charter codes.
    for lineno, line in lines:
        m = (
            CODENAME.search(line)
            or _ACCEPTANCE_CODES.search(line)
            or _PHASE_WORDS.search(line)
        )
        if m:
            where = "subject" if lineno == 1 else f"body line {lineno}"
            print(
                f"ERROR: commit message contains internal codename "
                f"'{m.group(0)}' ({where}). Write the concrete change, not "
                f"the task/sprint/gate/journey/version ID.",
                file=sys.stderr,
            )
            return 1

    # 3. Comparison framing + product names - the same rules the .rs comment
    # gate enforces, applied to the commit log: the message describes this
    # design, not another implementation or product.
    prod = product_pattern()
    for lineno, line in lines:
        m = COMPARISON.search(line)
        if m is None and prod is not None:
            m = prod.search(line)
        if m:
            where = "subject" if lineno == 1 else f"body line {lineno}"
            print(
                f"ERROR: commit message contains '{m.group(0)}' ({where}). "
                f"Describe this design; do not measure it against another "
                f"implementation or name other products.",
                file=sys.stderr,
            )
            return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
