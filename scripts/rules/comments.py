"""Shared .rs comment-violation rules — single source for the write-time
hook (hook_rust.py) and the check-time gate (check_rs_comments.py).

The same 6-arm detector serves both so write-time and check-time cannot
drift: CJK, backtick, codename/stage ref, design-doc ref, own-crate name,
comparison framing. The detector yields (message, line_text); each caller
formats its own output envelope (file:line prefix vs. hook block).
"""
import ast
import io
import re
import tokenize
from pathlib import Path

# Codename / stage / internal-ref patterns banned from .rs comments.
# Case-sensitive on purpose: uppercase letter+digit is a task/sprint tag;
# lowercase p99/p95 (a perf percentile metric) does not match, so perf
# notes can keep their lowercase form.
CODENAME = re.compile(
    r"\b[DRAFP]\d+\b|\b[ST]\d+\b|\bSEC-\d+\b|\bBLK-\d+\b|\bstage\d+\b|§\S*"
    r"|\bBet\s+[A-Z](?:\.\d+|\+[A-Z])?"
    r"|\bB\d+-\d+\b|\bST-\d+\b|\b[A-Z]\d+-[A-Z]\d+\b"
    r"|\b(?:CTX|MEM|RSM|MDL|STS|RWD)-\d+\b"
    r"|\bX-\d+\b"
    r"|\bJ\d+\b"
    r"|\b\w+\.rs:\d+\b"
)

# The project's own crate name — banned from comments (describe concerns,
# not crates; the name may change).
OWN_NAME = re.compile(r"\b(?:houyicoder|hicoder)(?:[-_][a-z0-9]+)*")

# Document references — a comment must stand on its own. A pointer outside
# the code is one the reader may be unable to follow, and it rots on its own
# schedule. State the reasoning instead.
DOC_REF = re.compile(
    r"\bdocs/"
    r"|\b[Ss]print\s+\d+"
    r"|\bcontract\s+[A-Z]\d+\b"
    r"|\b(?:design|acceptance)\s+doc\b"
    r"|\b\w+(?:-\w+)+\.md\b"
)

# Comparison framing — a comment must describe this design, not measure it
# against another implementation. The euphemism forms ("the reference agent",
# "a reference vendor", "the canonical layout") are worse than naming a
# product outright: a reader cannot resolve them to anything. "the reference
# implementation" is allowed (generic phrase for a default impl); "a reference
# to X" / "as a reference" are allowed (only the agent/vendor/sdk qualifier
# forms trip). "mirror" as a wire-side parallel type and "mirrors the
# frontend's" as internal cross-crate correspondence are standard vocabulary
# and not banned — only "the reference" + a euphemism qualifier is. "surpass"
# is banned bare (all inflections): in a comment describing this design it
# almost necessarily presupposes a surpassed baseline the reader cannot
# resolve, and it is internal jargon AGENTS.md already forbids.
COMPARISON = re.compile(
    r"\bthe reference\b(?! implementation)"
    r"|\ba reference (?:agent|runner|impl|vendor|sdk|project)\b"
    r"|\bsurpass\w*\b"
    r"|\bthe canonical layout\b"
    r"|\ba prior project\b",
    re.IGNORECASE,
)


# A growth-only ratchet on Phase N in .rs comment lines. The baseline
# covers the dream-consolidation prompt's own four-phase structure described
# in comments -- runtime prompt text, legitimate. A new Phase N is a roadmap
# forward-reference the reader cannot resolve. Drift -- a legit ref deleted
# -- is allowed; fewer is not a regression. Counted on comment lines only; the
# prompt text in string literals is not a comment.
PHASE_REFS = re.compile(r"\bPhase\s+\d+\b")
PHASE_REF_BASELINE = 6


def phase_ref_count(root):
    """Every Phase N hit in an .rs comment line under root, as a list of
    rel_path/lineno/match triples so a breach can list sites. Whole-tree: the
    ratchet is a global count, not per-changed-file, so it lives in the
    make-check gate rather than the per-line write-time hook."""
    hits = []
    for rs in Path(root).glob("**/*.rs"):
        try:
            text = rs.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for i, line in enumerate(text.splitlines(), 1):
            if is_comment(line):
                for m in PHASE_REFS.finditer(line):
                    hits.append((str(rs.relative_to(root)), i, m.group(0)))
    return hits


def evaluate_phase_refs(count, baseline=PHASE_REF_BASELINE):
    """Growth-only: 0 when count <= baseline, 1 when count > baseline."""
    return 0 if count <= baseline else 1


def has_cjk(line: str) -> bool:
    """CJK ranges: Han, Hangul, Kana, full-width punctuation."""
    return any(
        0x2E80 <= ord(c) <= 0x9FFF
        or 0xAC00 <= ord(c) <= 0xD7AF
        or 0xF900 <= ord(c) <= 0xFAFF
        or 0xFF00 <= ord(c) <= 0xFFEF
        for c in line
    )


def is_comment(line: str) -> bool:
    return line.lstrip().startswith("//")


def line_violations(line: str, *, check_cjk: bool = True):
    """Yield (message, line_text) for each comment-rule arm a .rs comment
    line trips. Only inspects // comment lines. Callers format the output
    envelope (file:line prefix vs. hook block).

    check_cjk gates the CJK arm: the CJK rule is owned by check_no_cjk
    (cross-filetype) at L2, so check_rs_comments passes False to avoid
    duplication; hook_rust passes True (default) for write-time .rs
    intercept at L1."""
    if not is_comment(line):
        return
    stripped = line.strip()
    if check_cjk and has_cjk(line):
        yield ("raw CJK in .rs comment; use \\uXXXX escapes (AGENTS.md)", stripped)
    if "`" in line:
        yield ("backtick in .rs comment; write bare identifiers, no markup (AGENTS.md)", stripped)
    m = CODENAME.search(line)
    if m:
        yield (
            f"codename/stage ref '{m.group(0)}' in .rs comment; "
            f"describe in plain English (AGENTS.md)",
            stripped,
        )
    dm = DOC_REF.search(line)
    if dm:
        yield (
            f"doc reference '{dm.group(0)}' in .rs comment; a comment must "
            f"stand on its own, so state the reasoning instead of citing "
            f"where it is written (AGENTS.md)",
            stripped,
        )
    om = OWN_NAME.search(line)
    if om:
        yield (
            f"own crate/project name '{om.group(0)}' in .rs comment; "
            f"describe the concern, do not name crates (AGENTS.md)",
            stripped,
        )
    cm = COMPARISON.search(line)
    if cm:
        yield (
            f"comparison framing '{cm.group(0)}' in .rs comment; "
            f"describe this design, do not measure it against another "
            f"implementation (AGENTS.md)",
            stripped,
        )


def py_doc_lines(text: str):
    """Yield (lineno, line) for # comments (including inline) and docstring
    content lines in Python source. Uses the stdlib tokenizer + AST for
    accuracy: a hand-written triple-quote state machine misread a literal
    triple-quote sequence in code as a docstring boundary and missed every
    inline comment (code with a trailing comment) — both halves of the
    failure are why this uses tokenize.COMMENT + ast.get_docstring instead.
    Shared by check_no_cjk."""
    lines = text.splitlines()
    # Comments: tokenize yields a COMMENT token for every #, inline or not.
    try:
        for tok in tokenize.generate_tokens(io.StringIO(text).readline):
            if tok.type == tokenize.COMMENT:
                yield tok.start[0], tok.string
    except tokenize.TokenizeError:
        pass
    # Docstrings: walk the AST, yield the source lines each docstring spans.
    try:
        tree = ast.parse(text)
        for node in ast.walk(tree):
            if isinstance(node, (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                if (node.body and isinstance(node.body[0], ast.Expr)
                        and isinstance(node.body[0].value, ast.Constant)
                        and isinstance(node.body[0].value.value, str)):
                    expr = node.body[0]
                    start = expr.lineno
                    end = getattr(expr, "end_lineno", start)
                    for i in range(start, end + 1):
                        if 0 < i <= len(lines):
                            yield i, lines[i - 1]
    except SyntaxError:
        pass
