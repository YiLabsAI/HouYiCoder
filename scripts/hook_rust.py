#!/usr/bin/env python3
"""PreToolUse hook: block Edit/Write/NotebookEdit to .rs files whose NEW
content violates the .rs comment rules (backticks, CJK, codenames,
design-doc refs) BEFORE the write lands. This is write-time intercept —
the violating content never reaches disk, no wasted turn.

Reads JSON from stdin: {"tool_name": ..., "tool_input": {...}}.
Exit 0 = allow. Exit 2 = block (stderr shown to the model so it can rewrite).

The comment-rule detector is shared with the check-time gate
(check_rs_comments.py) via rules/comments.py — one word source, no parallel
wordlist to drift. Test-name limits are shared via rules/naming.py.
"""
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.comments import line_violations  # noqa: E402
from rules.naming import (  # noqa: E402
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
_BASELINE_FILES = {"check_struct_fields.py", "check_file_size.py"}
_BASELINE_COMMENT = re.compile(
    r"^(STRUCT_FIELD_BASELINE|EXCESS_BASELINE)\s*=\s*\d+\s*#"
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
    """Yield (rule, basename) for test file names that violate (e.g. a new
    file written as foo_tests2.rs). Numbered suffixes are meaningless; use a
    descriptive scenario name per the sibling-test convention."""
    base = Path(path).name
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


def main():
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        sys.exit(0)
    tool = payload.get("tool_name", "")
    if tool not in ("Edit", "Write", "MultiEdit", "NotebookEdit"):
        sys.exit(0)
    ti = payload.get("tool_input", {})
    path = ti.get("file_path") or ti.get("notebook_path") or ""
    is_rs = str(path).endswith(".rs")
    is_baseline_py = Path(path).name in _BASELINE_FILES
    if not is_rs and not is_baseline_py:
        sys.exit(0)
    # Gather the new content being written.
    chunks = []
    if tool == "Write":
        chunks.append(ti.get("content", ""))
    elif tool == "Edit":
        chunks.append(ti.get("new_string", ""))
    elif tool == "MultiEdit":
        for e in ti.get("edits", []):
            chunks.append(e.get("new_string", ""))
    elif tool == "NotebookEdit":
        chunks.append(ti.get("new_source", ""))
    seen = set()
    for chunk in chunks:
        if is_rs:
            for line in chunk.splitlines():
                for message, line_text in line_violations(line):
                    key = (message, line_text)
                    if key in seen:
                        continue
                    seen.add(key)
                    print(f"blocked write to {path}: {message}\n  -> {line_text}", file=sys.stderr)
            for rule, name in _test_name_violations(chunk):
                key = (rule, name)
                if key in seen:
                    continue
                seen.add(key)
                print(f"blocked write to {path}: {rule}\n  -> {name}", file=sys.stderr)
            for message, preview in _comment_density_violations(chunk):
                key = (message, preview)
                if key in seen:
                    continue
                seen.add(key)
                print(f"blocked write to {path}: {message}\n  -> {preview}", file=sys.stderr)
        if is_baseline_py:
            for message, line in _baseline_comment_violation(chunk):
                key = (message, line)
                if key in seen:
                    continue
                seen.add(key)
                print(f"blocked write to {path}: {message}\n  -> {line}", file=sys.stderr)
    for rule, name in _file_name_violation(path):
        key = (rule, name)
        if key in seen:
            continue
        seen.add(key)
        print(f"blocked write to {path}: {rule}\n  -> {name}", file=sys.stderr)
    if seen:
        print(
            "\nRewrite: plain English comments (no product names / backticks "
            "/ CJK / codenames / design-doc refs) and descriptive test names "
            "(no test1/test2, no e2e/integration type tags -- the directory "
            "denotes the type) (AGENTS.md).",
            file=sys.stderr,
        )
        sys.exit(2)
    sys.exit(0)


if __name__ == "__main__":
    main()
