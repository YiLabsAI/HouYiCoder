#!/usr/bin/env python3
"""PreToolUse hook: block Edit/Write/NotebookEdit to .rs files whose NEW
content violates the .rs comment rules (backticks, CJK, codenames,
design-doc refs) BEFORE the write lands. This is write-time intercept —
the violating content never reaches disk, no wasted turn.

Reads JSON from stdin. Two payload shapes reach this script and both must
gate: this product's own PreToolUse context names the tool in tool_name and
carries its arguments under input, with the tool's own schema keys (path,
content, old_string, edits); the external harness shape carries tool_input
with file_path. The arguments container and the path key are the only
differences, so one normalizer covers both.
Exit 0 = allow. Exit 2 = block (stderr shown to the model so it can rewrite).

The comment-rule detector is shared with the check-time gate
(check_rs_comments.py) via rules/comments.py — one word source, no parallel
wordlist to drift. Test-name limits are shared via rules/naming.py.
"""
import json
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.comments import line_violations  # noqa: E402
from rules.imports import inline_use_violations, repeated_deep_paths  # noqa: E402
from rules.naming import (  # noqa: E402
    ALLOWED_MOD_RS,
    ALLOWED_TOP_MODULES,
    JARGON,
    TEST_NAME_LEN_HARD,
    TEST_UNDERSCORE_CAP,
)

# Test-name gate: a test fn must be descriptive, not numbered-meaningless
# (test1, test2) and not type-tagged (e2e, integration) -- the directory
# structure already denotes the test type (tests/ = e2e/integration,
# src/*_tests.rs = cross-file unit). Applies to the fn name AND the file
# basename so a new file like foo_tests2.rs is blocked at write time too.
_TEST_FN = re.compile(r"\bfn\s+(test\w+)\s*\(")
_NUMBERED_TEST = re.compile(r"^test\d+$")
_TYPE_TAG = re.compile(r"(?:e2e|integration)", re.IGNORECASE)
_NUMBERED_TEST_FILE = re.compile(r"_tests?\d+\.rs$")

_COMMENT_DENSITY_CAP = 8
_BASELINE_FILES = {"monitored_structs.py", "check_file_size.py"}
_BASELINE_COMMENT = re.compile(
    r'^\s*"(?:fields|mut_app|EXCESS_BASELINE)":\s*\d+,\s*#'
)


def _test_name_violations(text: str):
    """Yield (rule, name) for test fn names that violate the naming rules."""
    for m in _TEST_FN.finditer(text):
        name = m.group(1)
        if _NUMBERED_TEST.match(name):
            yield (
                f"meaningless numbered test name '{name}'; describe the behavior",
                name,
            )
        tm = _TYPE_TAG.search(name)
        if tm:
            yield (
                f"type tag '{tm.group(0)}' in test name '{name}'; "
                f"the directory already denotes the test type",
                name,
            )
        if name.count("_") > TEST_UNDERSCORE_CAP:
            yield (
                f"test name '{name}' has >{TEST_UNDERSCORE_CAP} underscores; "
                f"trim or move detail into a doc",
                name,
            )
        if len(name) > TEST_NAME_LEN_HARD:
            yield (
                f"test name '{name}' is >{TEST_NAME_LEN_HARD} chars; trim",
                name,
            )
        jm = JARGON.search(name)
        if jm:
            yield (
                f"jargon term '{jm.group(0)}' in test name '{name}'; use "
                f"program-design / distributed-systems vocabulary "
                f"(control plane / data plane, concurrent, active) "
                f"instead of ad-hoc terms",
                name,
            )


def _file_name_violation(path: str):
    """Yield (rule, basename) for test file names and directory layout rules."""
    p = Path(path)
    base = p.name
    if _NUMBERED_TEST_FILE.search(base):
        yield (
            f"numbered test file name '{base}'; use a descriptive scenario name",
            base,
        )
    jm = JARGON.search(base)
    if jm:
        yield (
            f"jargon term '{jm.group(0)}' in test file name '{base}'; use "
            f"program-design / distributed-systems vocabulary "
            f"(control plane / data plane, concurrent, active) "
            f"instead of ad-hoc terms",
            base,
        )
    posix = p.as_posix()
    idx = posix.find("crates/")
    if idx >= 0:
        rel = posix[idx:]
        parent_rel = str(Path(rel).parent).replace("\\", "/")
        if parent_rel in ALLOWED_TOP_MODULES:
            stem = Path(rel).stem
            if stem not in ALLOWED_TOP_MODULES[parent_rel]:
                yield (
                    f"top-level module '{stem}' is not in ALLOWED_TOP_MODULES[{parent_rel!r}]; "
                    f"place the file inside its owning domain directory",
                    rel,
                )
        if "/src/" in rel:
            if base == "mod.rs" and rel not in ALLOWED_MOD_RS:
                yield (
                    "unexpected production mod.rs file; use the Rust 2018 form X.rs + X/",
                    rel,
                )
            elif p.stem == p.parent.name:
                yield (
                    f"module self-stutter ({p.parent.name}/{base}); put the entry at "
                    f"{p.parent.name}.rs beside {p.parent.name}/",
                    rel,
                )
            elif base.endswith("_tests.rs"):
                yield (
                    "flat *_tests.rs file in src/; use X/tests.rs instead",
                    rel,
                )


def _comment_density_violations(text: str):
    count = 0
    preview = ""
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("//") and not s.startswith("///"):
            count += 1
            if count == 1:
                preview = s[:60]
        else:
            if count > _COMMENT_DENSITY_CAP:
                yield ("comment block too dense; trim to essentials", preview)
            count = 0
    if count > _COMMENT_DENSITY_CAP:
        yield ("comment block too dense; trim to essentials", preview)


def _baseline_comment_violation(text: str):
    for line in text.splitlines():
        if _BASELINE_COMMENT.match(line):
            yield ("baseline bump must not add a trailing comment", line)


# Tool names the gate covers, in both spellings that reach it: this product's
# tools are lowercase (write, edit, multiedit) and the external harness
# capitalizes them. Keying on the lowercased name keeps one set.
_WRITE_TOOLS = {"write", "edit", "multiedit", "notebookedit"}

# Path key candidates in order: this product's schema uses path, the external
# harness uses file_path (notebook_path for a notebook).
_PATH_KEYS = ("path", "file_path", "notebook_path")


def _text(value) -> str:
    """The content field as text, whatever the payload put there."""
    return value if isinstance(value, str) else ""


def _normalize(payload: dict):
    """Return (tool, args, path) for either payload shape, or None to skip.

    The arguments container differs (input here, tool_input externally) and
    so does the path key, so both are resolved here rather than at each use.
    A payload naming no tool is not a write an editor issued, so it is
    skipped; one naming a covered tool but carving out no path is blocked by
    the caller instead, since a write the gate cannot locate is the case it
    must not wave through. Bash is admitted here so its heredoc bodies reach
    the same rust checks; its path is parsed out of the command, not a key.
    """
    tool = str(payload.get("tool_name") or "").lower()
    if tool not in _WRITE_TOOLS and tool != "bash":
        return None
    args = payload.get("input")
    if not isinstance(args, dict):
        args = payload.get("tool_input")
    if not isinstance(args, dict):
        args = {}
    path = ""
    if tool in _WRITE_TOOLS:
        for key in _PATH_KEYS:
            value = args.get(key)
            if isinstance(value, str) and value:
                path = value
                break
    return tool, args, path


_REWRITE_HINT = (
    "\nRewrite: plain English comments (no product names / backticks "
    "/ CJK / codenames / design-doc refs) and descriptive test names "
    "(no test1/test2, no e2e/integration type tags -- the directory "
    "denotes the type) (AGENTS.md)."
)


def _rust_violations(path, chunk, existing_lines, existing_paths):
    """Yield (rule, detail) for one chunk of rust source being written.

    The disk sets (existing_lines, existing_paths) carry over violations the
    file already holds, so a rewrite or a move is not reported as new. The
    caller prints and dedups what this yields.
    """
    if not chunk:
        return
    for line in chunk.splitlines():
        for message, line_text in line_violations(line):
            yield message, line_text
    for rule, name in _test_name_violations(chunk):
        yield rule, name
    for message, preview in _comment_density_violations(chunk):
        yield message, preview
    for _lineno, line in inline_use_violations(chunk):
        if line in existing_lines:
            continue
        yield (
            "a use statement inside a function body; move it to the "
            "module's import block",
            line,
        )
    for chain in repeated_deep_paths(chunk):
        if chain in existing_paths:
            continue
        yield (
            "deep qualified path in code body; import it at module "
            "scope instead of repeating or nesting :: chains",
            chain,
        )


# A heredoc opener that writes a .rs file: cat/tee with a redirect to a .rs
# path followed by a quoted or bare heredoc delimiter, or the delimiter
# followed by the redirect. Both orderings are valid shell syntax.
_HEREDOC_OPEN = re.compile(
    r"(?:^|[;&|]\s*|&&\s*)"
    r"(?:cat|tee)\b.*?"
    r"(?:(?:>>|>)\s*(\S+\.rs)\s*<<-?\s*['\"]?(\w+)['\"]?|<<-?\s*['\"]?(\w+)['\"]?\s*(?:>>|>)\s*(\S+\.rs))"
    r"\s*$"
)


def _bash_rs_heredocs(command):
    """Return [(rs_path, body)] for each heredoc writing a .rs file.

    A bash command that writes rust via a heredoc carries the new source in
    the body between the opener line and the closing delimiter. Both ordering
    conventions (cat >> file <<EOF and cat <<EOF > file) are recognized.
    """
    if not command:
        return []
    lines = command.splitlines()
    results = []
    i = 0
    while i < len(lines):
        m = _HEREDOC_OPEN.search(lines[i])
        if not m:
            i += 1
            continue
        if m.group(1) is not None:
            rs_path, delim = m.group(1), m.group(2)
        else:
            delim, rs_path = m.group(3), m.group(4)
        body_lines = []
        i += 1
        while i < len(lines):
            if lines[i].strip() == delim:
                break
            body_lines.append(lines[i])
            i += 1
        results.append((rs_path, "\n".join(body_lines)))
        i += 1
    return results


# A bash command that rewrites a workspace source file through an
# interpreter instead of the edit tool. It bypasses every static check, the
# file transaction ledger that undo reads, and the structured diff, so the
# write is refused rather than inspected: the body of a python heredoc is not
# a .rs file this gate can parse as one.
_SCRIPT_EDITORS = re.compile(r"(?:^|[;&|]\s*|&&\s*|\|\|\s*|\$\(\s*)(python3?|perl|sed)\s")


def _script_edit_targets(command):
    """Return the workspace .rs paths a scripted edit would rewrite.

    Recognizes the three shapes that write a file from a shell command:
    python with a write-mode open or a write_text call, sed with an in-place
    flag, and perl with an in-place flag. A path under the temp directory is
    left alone: scratch fixtures are not workspace source, and a test that
    writes one is not editing the tree.
    """
    if not command:
        return []
    if not _SCRIPT_EDITORS.search(command):
        return []
    writes = (
        "write_text" in command
        or re.search(r"open\s*\([^)]*['\"][wa]", command) is not None
        or re.search(r"\bsed\s+[^;|&]*-i", command) is not None
        or re.search(r"\bperl\s+[^;|&]*-i", command) is not None
    )
    if not writes:
        return []
    targets = []
    for token in re.findall(r"[\w./~$-]*\.rs\b", command):
        if _is_scratch_path(token):
            continue
        targets.append(token)
    return targets


def _is_scratch_path(path):
    """Whether a path names a temp location rather than the workspace."""
    scratch_roots = ("/tmp/", "/private/tmp/")
    if path.startswith(scratch_roots):
        return True
    for key in ("TMPDIR", "TMP"):
        root = os.environ.get(key)
        if root and path.startswith(root.rstrip("/") + "/"):
            return True
    return False


def main():
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        sys.exit(0)
    if not isinstance(payload, dict):
        sys.exit(0)
    normalized = _normalize(payload)
    if normalized is None:
        sys.exit(0)
    tool, ti, path = normalized

    if tool == "bash":
        command = _text(ti.get("command"))
        scripted = _script_edit_targets(command)
        if scripted:
            for rs_path in sorted(set(scripted)):
                print(
                    f"blocked bash script edit of workspace .rs file {rs_path}: "
                    "use the edit, multiedit, or write tool so static checks, "
                    "the file transaction ledger, and the structured diff apply.",
                    file=sys.stderr,
                )
            sys.exit(2)
        targets = _bash_rs_heredocs(command)
        if not targets:
            # A bash command with no .rs heredoc the parser can read is not a
            # rust write this gate can see, so it lets the call through.
            sys.exit(0)
        seen = set()
        for rs_path, body in targets:
            existing_lines = set()
            existing_paths = set()
            try:
                disk_text = Path(rs_path).read_text(encoding="utf-8")
                existing_lines = {line.strip() for line in disk_text.splitlines()}
                existing_paths = set(repeated_deep_paths(disk_text))
            except OSError:
                pass
            for rule, detail in _rust_violations(
                rs_path, body, existing_lines, existing_paths
            ):
                key = (rule, detail)
                if key in seen:
                    continue
                seen.add(key)
                print(
                    f"blocked write to {rs_path}: {rule}\n  -> {detail}",
                    file=sys.stderr,
                )
            for rule, name in _file_name_violation(rs_path):
                key = (rule, name)
                if key in seen:
                    continue
                seen.add(key)
                print(
                    f"blocked write to {rs_path}: {rule}\n  -> {name}",
                    file=sys.stderr,
                )
        if seen:
            print(_REWRITE_HINT, file=sys.stderr)
            sys.exit(2)
        sys.exit(0)

    if not path:
        # A covered tool with no locatable target: the file the rules apply to
        # is unknown, so allowing would be a silent hole.
        print(
            f"blocked {tool}: no path in the hook payload, so the comment "
            "rules cannot be applied",
            file=sys.stderr,
        )
        sys.exit(2)
    is_rs = str(path).endswith(".rs")
    is_baseline_py = Path(path).name in _BASELINE_FILES
    if not is_rs and not is_baseline_py:
        sys.exit(0)
    # Gather the new content being written. The tool name is already lowercased
    # by the normalizer, so one branch set covers both payload shapes.
    chunks = []
    if tool == "write":
        chunks.append(_text(ti.get("content")))
    elif tool == "edit":
        chunks.append(_text(ti.get("new_string")))
    elif tool == "multiedit":
        edits = ti.get("edits")
        for e in edits if isinstance(edits, list) else []:
            if isinstance(e, dict):
                chunks.append(_text(e.get("new_string")))
    elif tool == "notebookedit":
        chunks.append(_text(ti.get("new_source")))
    seen = set()
    # The lines and deep paths already on disk, so a carried-over violation is
    # not reported as a new one.
    existing_lines = set()
    existing_paths = set()
    try:
        disk_text = Path(path).read_text(encoding="utf-8")
        existing_lines = {line.strip() for line in disk_text.splitlines()}
        existing_paths = set(repeated_deep_paths(disk_text))
    except OSError:
        pass
    for chunk in chunks:
        if is_rs:
            for rule, detail in _rust_violations(
                path, chunk, existing_lines, existing_paths
            ):
                key = (rule, detail)
                if key in seen:
                    continue
                seen.add(key)
                print(
                    f"blocked write to {path}: {rule}\n  -> {detail}",
                    file=sys.stderr,
                )
        if is_baseline_py:
            for message, line in _baseline_comment_violation(chunk):
                key = (message, line)
                if key in seen:
                    continue
                seen.add(key)
                print(
                    f"blocked write to {path}: {message}\n  -> {line}",
                    file=sys.stderr,
                )
    for rule, name in _file_name_violation(path):
        key = (rule, name)
        if key in seen:
            continue
        seen.add(key)
        print(f"blocked write to {path}: {rule}\n  -> {name}", file=sys.stderr)
    if seen:
        print(_REWRITE_HINT, file=sys.stderr)
        sys.exit(2)
    sys.exit(0)


if __name__ == "__main__":
    main()
