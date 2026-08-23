#!/usr/bin/env python3
"""Console-write gate: keep print macros out of the interactive runtime.

The TUI runs in the terminal's alternate screen. That screen does not
capture stderr or stdout, so a write from library code is painted by the
terminal at wherever the cursor happens to sit -- which, during a session,
is inside the input box. A best-effort failure reported with eprintln does
not reach a log; it corrupts the surface the user is typing into.

There are three sinks and the choice between them is a design decision,
not a matter of taste:

  user-visible   the user must know, or can act on it. Emit a system line
                 (LiveEvent::SystemLine through the runner's live sink);
                 it lands in the transcript and survives scrollback.
  diagnostic     only a developer can use it. Append to the debug log
                 (the debug_log module, gated by HOUYICODER_DEBUG_LOG);
                 it goes to a file and never touches the terminal.
  console        no alternate screen is up: argument parsing, startup
                 failures, a non-TUI binary. A print macro is correct here
                 and only here.

The distinction was already known -- a comment in the cli composition root
records it for one call site -- but a comment guards one line, so the rest
of the tree kept writing to the console. This gate is the mechanical form
of that comment.

Two lists carry the classification, both as path -> count:

  _CONSOLE_OK        writes that ARE the console sink. Permanent, not a
                     ratchet: these are correct and stay.
  _STDERR_BASELINE   pre-existing runtime writes awaiting migration to a
                     system line or the debug log. Ratchets to empty.

Counted per file rather than exempting a file outright, because the two
classes share files. The cli entry point holds argument parsing (console,
correct) and the TUI wiring that runs after the alternate screen is up
(not correct) in the same module; a whole-file exemption would bless
exactly the call sites that need guarding. A count makes a new write in
either group a deliberate edit to this table.

Counts, not line numbers: a line number goes stale on any edit above it,
which would turn the table into churn and train everyone to regenerate it
instead of reading it. A count still blocks the case that matters (a NEW
write in a file that already has some) while surviving refactors.

Both tables are matched EXACTLY, so a count going down is an error too.
Tolerating a drop would let the table drift above the truth, and a ratchet
allowed to be wrong in the loose direction stops being evidence of
anything -- the same reason a stale flat-prefix line is an error rather
than a silent pass.

Test and example targets are exempt by structure, not by listing: their
output IS the product (a skip notice, a benchmark number, a rendered-screen
dump read by a human running the test). The exemption covers tests/ and
examples/ trees, *_tests.rs peer files, and the conventional trailing
#[cfg(test)] mod in a source file.

Run: python3 scripts/check_stderr.py  (wired into make check as stderr).
Exit 1 on a violation, 0 otherwise.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES = ROOT / "crates"

# Console writes that are the correct sink: no alternate screen is up when
# they run. Permanent entries, NOT a ratchet -- do not migrate these to a
# system line (there is no transcript yet) or to the debug log (the user
# needs to see why the binary refused to start, on the terminal, now).
#
# The cli entry point covers argument-parse failures, the session list that
# is the ls subcommand's whole output, and the id/pid/socket banner the
# detached and ACP server modes print for a client to read back. The uds
# listener only runs in detached mode, where this process has no TUI.
_CONSOLE_OK: dict[str, int] = {
    "crates/houyicoder-cli/src/cli_args.rs": 1,
    "crates/houyicoder-cli/src/cleanup.rs": 12,
    "crates/houyicoder-cli/src/main.rs": 16,
    "crates/houyicoder-cli/src/resume_bundle.rs": 2,
    "crates/houyicoder-loader/src/main.rs": 2,
    "crates/houyicoder-service/src/uds.rs": 2,
}

# Pre-existing runtime console writes. Each reaches the terminal while the
# TUI may own the screen. Migrate to a system line (the user must know) or
# the debug log (diagnostic only), and lower the count in the same commit
# so the ratchet stays honest.
#
# The sandbox entries are the loudest of these: an unenforced-fence audit
# notice, per operation, on the platforms whose fence can be unavailable.
# That is a security fact the user must be told, and the console tells it
# by writing over whatever the terminal was showing.
_STDERR_BASELINE: dict[str, int] = {}

# eprintln / println / eprint / print, as a macro call, not as part of a
# longer identifier and not as a method (.print!). Matching the bang keeps
# a fn named print_summary out of the result.
_PRINT_RE = re.compile(r"(?<![\w.])e?print(?:ln)?!")

# The conventional trailing test module: #[cfg(test)] on its own line
# followed by a mod declaration. Matched as a pair so a #[cfg(test)] on a
# use statement or a single fn does not blank out the rest of the file.
_CFG_TEST_RE = re.compile(r"^#\[cfg\(test\)\]\s*$")
_MOD_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s")


def _is_exempt_path(rel: str) -> bool:
    """True for targets whose console output is the product: integration
    test binaries, example binaries, and peer-test module files."""
    parts = rel.split("/")
    if "tests" in parts or "examples" in parts or "benches" in parts:
        return True
    return parts[-1].endswith("_tests.rs")


def _test_mod_cutoff(lines: list[str]) -> int:
    """Index of the first line of the trailing #[cfg(test)] mod, or len(lines)
    when the file has none. Writes at or after the cutoff are test output."""
    for i, line in enumerate(lines):
        if not _CFG_TEST_RE.match(line):
            continue
        # Skip any further attributes between the cfg and the mod keyword.
        j = i + 1
        while j < len(lines) and lines[j].lstrip().startswith("#["):
            j += 1
        if j < len(lines) and _MOD_RE.match(lines[j]):
            return i
    return len(lines)


def count_writes(path: Path) -> int:
    """Console writes in a source file, excluding its trailing test module."""
    lines = path.read_text(encoding="utf-8").splitlines()
    cutoff = _test_mod_cutoff(lines)
    return sum(1 for line in lines[:cutoff] if _PRINT_RE.search(line))


def scan(root: Path = CRATES) -> dict[str, int]:
    """Console writes per file across the tree, structurally exempt targets
    dropped and zero-count files omitted. Keys are paths relative to the
    parent of root, so scanning the real crates directory yields the
    crates/... paths the tables are written in."""
    found: dict[str, int] = {}
    for p in sorted(root.rglob("*.rs")):
        if _is_exempt_path(p.relative_to(root).as_posix()):
            continue
        n = count_writes(p)
        if n:
            found[p.relative_to(root.parent).as_posix()] = n
    return found


def check(
    found: dict[str, int],
    console_ok: dict[str, int] | None = None,
    baseline: dict[str, int] | None = None,
) -> list[str]:
    """Compare a scan against the two tables and return the error lines.
    Both tables are matched exactly: a file may hold both a correct console
    write and a runtime one, so the allowance for a file is the sum of its
    two entries and any deviation is reported."""
    console_ok = _CONSOLE_OK if console_ok is None else console_ok
    baseline = _STDERR_BASELINE if baseline is None else baseline
    errors: list[str] = []
    for rel in sorted(set(found) | set(console_ok) | set(baseline)):
        n = found.get(rel, 0)
        ok = console_ok.get(rel, 0)
        listed = baseline.get(rel, 0)
        allowed = ok + listed
        if n == allowed:
            continue
        if n > allowed:
            errors.append(
                f"{rel}: {n} console write(s), {allowed} allowed. A print "
                f"macro reaches the terminal, and while the TUI owns the "
                f"screen the cursor sits in the input box -- the text lands "
                f"there. Emit a system line if the user must know, or use "
                f"the debug log if it is diagnostic. If this call site runs "
                f"before the alternate screen is up, raise the _CONSOLE_OK "
                f"count instead."
            )
        else:
            errors.append(
                f"{rel}: tables allow {allowed} console write(s), found {n}. "
                f"Lower the count in the same commit that removes a write, "
                f"so the ratchet stays honest."
            )
    return errors


def main() -> int:
    found = scan()
    errors = check(found)
    if errors:
        print("stderr-gate: violations")
        for e in errors:
            print(f"  {e}")
        print(f"\n{len(errors)} violation(s)")
        return 1
    remaining = sum(_STDERR_BASELINE.values())
    if remaining:
        print(
            f"[stderr] {remaining} pre-existing console write(s) in "
            f"{len(_STDERR_BASELINE)} file(s) tolerated "
            f"(see _STDERR_BASELINE); ratchet down as each migrates.",
            file=sys.stderr,
        )
    print("stderr-gate: clean — no new console writes in runtime code")
    return 0


if __name__ == "__main__":
    sys.exit(main())
