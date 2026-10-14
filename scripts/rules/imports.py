"""Where imports belong, and how deep a qualified path may repeat.

Two rules, one source, shared by the write-time hook (hook_rust.py) and the
commit-time ratchet (check_rust_imports.py):

  * A use statement belongs at module scope. Inside a function body it is a
    local import: it hides where the dependency enters the module and it is
    invisible to anyone reading the import block.
  * A qualified path with two or more separators belongs in the import block
    once it is used twice in the same file. A single use is left alone: a
    one-off or a deliberate disambiguation is not a finding.

The second rule is a repeat rule, not a depth limit: it counts how often one
importable chain appears, and one appearance is allowed.
"""
import re

# A qualified path with at least two separators, e.g. crate::state::App or
# std::time::Duration::from_secs. One separator (fmt::Display) is allowed.
_DEEP_PATH = re.compile(
    r"\b([A-Za-z_][A-Za-z0-9_]*::[A-Za-z_][A-Za-z0-9_]*::[A-Za-z_][A-Za-z0-9_:]*)"
)
_USE = re.compile(r"\buse\b")
# A fn declaration in any of the forms this workspace uses: visibility
# modifiers (pub, pub(crate), pub(super), pub(in path)), qualifiers (async,
# unsafe, const, extern "C"), and a body that may open lines after the name.
_FN_HEAD = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)*(?:(?:async|unsafe|const|extern\s+\"[^\"]*\")\s+)*fn\b")
# A char literal closes within a few characters; a lifetime like 'a does not.
_CHAR_LITERAL = re.compile(r"'(?:\\.|[^\\'])'")


def _strip_line(line):
    """Drop the trailing comment and any string or char literal content, so
    braces and paths inside them cannot be counted."""
    out = []
    in_str = False
    escaped = False
    i = 0
    while i < len(line):
        ch = line[i]
        if in_str:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_str = False
            i += 1
            continue
        if ch == '"':
            in_str = True
            i += 1
            continue
        if ch == "/" and i + 1 < len(line) and line[i + 1] == "/":
            break
        if ch == "'":
            literal = _CHAR_LITERAL.match(line, i)
            if literal:
                i = literal.end()
                continue
        out.append(ch)
        i += 1
    return "".join(out)


def _fn_depths(text):
    """Yield (lineno, raw_line, inside_fn) for every line.

    inside_fn is true when the innermost open block was opened by a fn
    declaration, including a declaration whose brace lands on a later line.
    Strings, char literals and trailing comments are removed before braces
    are counted, and a lifetime is not mistaken for a char literal.
    """
    stack = []
    pending_fn = False
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = _strip_line(raw)
        stripped = line.strip()
        if pending_fn and "{" in line:
            # The body of a multi-line signature opens here.
            stack.append("fn")
            pending_fn = False
        if _FN_HEAD.match(stripped):
            if "{" in line.split("(", 1)[-1] or line.rstrip().endswith("{"):
                stack.append("fn")
            else:
                pending_fn = True
        else:
            for _ in range(line.count("{")):
                stack.append("other")
        in_fn = "fn" in stack
        yield lineno, raw, in_fn
        for _ in range(min(line.count("}"), len(stack))):
            stack.pop()


def inline_use_violations(text):
    """Yield (lineno, stripped_line) for a use statement inside a function.

    A one-line body (fn f() { use x; }) counts: the use sits after the
    opening brace on the same line.
    """
    for lineno, raw, in_fn in _fn_depths(text):
        code = _strip_line(raw)
        if not _USE.search(code):
            continue
        stripped = raw.strip()
        if in_fn:
            # The whole line is inside a body, or a body opened on this line
            # and the use follows the brace.
            if _FN_HEAD.match(code.strip()) and "{" not in code:
                continue
            yield lineno, stripped


def _importable(path):
    """The part of a qualified path an import would bring in.

    A trailing variant or associated item is not part of the import: the
    reader imports the type and writes Screen::Focus at the call site, so
    the two spellings of one type group together. A path with no uppercase
    segment after the crate root is a function or constant, imported whole.
    """
    segments = path.split("::")
    for i, segment in enumerate(segments):
        if segment[:1].isupper():
            return "::".join(segments[: i + 1])
    return path


def repeated_deep_paths(text):
    """Map each importable path used twice or more to its use count.

    Import lines and comments do not count: the rule is about a path
    repeated in fields, signatures or bodies instead of imported once.
    """
    counts = {}
    for _, raw, _ in _fn_depths(text):
        code = _strip_line(raw)
        stripped = code.strip()
        if not stripped or stripped.startswith("//") or re.match(r"^(?:pub\s+)?use\b", stripped):
            continue
        for path in _DEEP_PATH.findall(code):
            key = _importable(path)
            counts[key] = counts.get(key, 0) + 1
    return {path: n for path, n in counts.items() if n >= 2}
