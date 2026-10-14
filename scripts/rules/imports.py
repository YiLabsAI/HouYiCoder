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
    r"\b([A-Za-z_][A-Za-z0-9_]*::[A-Za-z_][A-Za-z0-9_]*::[A-Za-z_][A-Za-z0-9_]*"
    r"(?:::[A-Za-z_][A-Za-z0-9_]*)*)"
)
_USE_HEAD = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?use\b")
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


def _lines(text):
    """Yield (lineno, raw_line, inside_fn, inside_use) for every line.

    inside_fn is true when the innermost open block was opened by a fn
    declaration, including a declaration whose brace lands on a later line.
    inside_use is true for every line of a use statement, including the
    continuation lines of a grouped import, so those lines are never read
    as code. Strings, char literals and trailing comments are removed
    before braces are counted, and a lifetime is not mistaken for a char
    literal.
    """
    stack = []
    pending_fn = False
    in_use = False
    use_depth = 0
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = _strip_line(raw)
        stripped = line.strip()
        if in_use:
            use_depth += line.count("{") - line.count("}")
            if use_depth <= 0 and stripped.endswith(";"):
                in_use = False
            yield lineno, raw, "fn" in stack, True
            continue
        starts_use = bool(_USE_HEAD.match(stripped))
        if starts_use:
            use_depth = line.count("{") - line.count("}")
            in_use = use_depth > 0 or not stripped.endswith(";")
            yield lineno, raw, "fn" in stack, True
            continue
        if pending_fn and "{" in line:
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
        yield lineno, raw, in_fn, False
        if stripped == "}" and raw[:1] not in (" ", "\t"):
            # A brace at column zero closes the outermost item, so any block
            # still tracked is the tail of an earlier miscount.
            stack.clear()
        else:
            for _ in range(min(line.count("}"), len(stack))):
                stack.pop()


def inline_use_violations(text):
    """Yield (lineno, stripped_line) for a use statement inside a function.

    A use at column zero is module scope by definition (rustfmt indents every
    statement in a body), so the brace walk cannot produce a false positive
    on it. A one-line body (fn f() { use x; }) counts: the use sits after the
    opening brace on the same line, so the line is not a plain import.
    """
    for lineno, raw, in_fn, in_use in _lines(text):
        code = _strip_line(raw)
        stripped = code.strip()
        if raw[:1] not in (" ", "\t") and not _FN_HEAD.match(stripped):
            continue
        if in_use:
            if not _USE_HEAD.match(stripped):
                continue
            if in_fn:
                yield lineno, raw.strip()
            continue
        if _FN_HEAD.match(stripped) and re.search(r"\{\s*use\b", code):
            yield lineno, raw.strip()


def _importable(path):
    """The part of a qualified path an import would bring in, or None.

    Any path with two or more :: separators starting with a module/crate
    name (lowercase first segment) has at least one unimported module prefix:
    - mpsc::TryRecvError::Empty -> import mpsc::TryRecvError, write
      TryRecvError::Empty (one ::);
    - crate::state::Screen::Working -> import crate::state::Screen, write
      Screen::Working (one ::);
    - crate::app::handle_key -> import crate::app::handle_key, write
      handle_key (zero ::).
    """
    segments = path.split("::")
    if len(segments) < 3 or not segments[0][:1].islower():
        return None
    for i, segment in enumerate(segments):
        if segment[:1].isupper():
            return "::".join(segments[: i + 1])
    return path


def deep_path_violations(text):
    """Yield (lineno, importable_path) for every unimported multi-module path
    in code lines (outside use statements and comments)."""
    for lineno, raw, _, in_use in _lines(text):
        if in_use:
            continue
        code = _strip_line(raw)
        stripped = code.strip()
        if not stripped or stripped.startswith("//"):
            continue
        for path in _DEEP_PATH.findall(code):
            key = _importable(path)
            if key is not None:
                yield lineno, key


def repeated_deep_paths(text):
    """Map each unimported deep path to its occurrence count when it violates.

    A path with two or more module segments before the final item
    (crate::state::Screen, std::time::UNIX_EPOCH, crate::app::handle_key)
    belongs in the import block even when it appears once: the qualified
    spelling repeats a module the reader already has in scope, and a single
    occurrence is exactly how the next one starts. Two segments
    (SessionEvent::Variant, mpsc::TryRecvError) are a type or module
    qualifier, not a deep path.

    A name the file defines itself, or one two different chains end in, is
    left qualified: an import would collide or pick the wrong type, and the
    rules reserve a qualified path for exactly that case.
    """
    counts = {}
    for _, key in deep_path_violations(text):
        counts[key] = counts.get(key, 0) + 1
    by_name = {}
    for chain in counts:
        by_name.setdefault(chain.rsplit("::", 1)[-1], set()).add(chain)
    defined = set()
    for line in text.splitlines():
        m = re.match(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?"
            r"(?:fn|struct|enum|trait|union|const|static|type)\s+([A-Za-z0-9_]+)",
            line,
        )
        if m:
            defined.add(m.group(1))
    return {
        chain: n
        for chain, n in counts.items()
        if len(by_name[chain.rsplit("::", 1)[-1]]) == 1
        and chain.rsplit("::", 1)[-1] not in defined
    }
