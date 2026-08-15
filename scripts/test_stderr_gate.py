#!/usr/bin/env python3
"""Regression tests for the console-write gate (check_stderr.py).

The gate is the machine backstop for a rule that a comment already stated
and the tree ignored anyway -- do not write to the terminal from code that
runs under the alternate screen -- so the gate itself has to be guarded,
the same reason test_hook_rust.py and test_flat_prefix.py exist.

Every test is PAIRED: an exemption case is asserted alongside a real
violation in the same tree, and the real one must be the ONLY thing
reported. A test that only asserts "the exempt thing was not flagged" is
silent when the scanner returns nothing at all for an unrelated reason (a
path glob that matches nothing passes any not-flagged assertion), so each
exemption is proved against a positive control that must still fire.

Covered logics:
  - macro detection, and that an identifier merely containing print does
    not count
  - the trailing #[cfg(test)] mod cutoff, and that a #[cfg(test)] on a
    non-mod item does not blank the rest of the file
  - path exemptions (tests/, examples/, benches/, *_tests.rs)
  - all three check verdicts: exact match passes, over-count fires,
    under-count fires
  - the two tables sum for one file, so a mixed file is allowed exactly
    its console writes plus its baselined ones
  - the injection contract -- check must read its parameters, not the
    module globals

Run: python3 scripts/test_stderr_gate.py  (wired into make check as
stderr-gate-tests). Exit 0 = pass, 1 = fail.
"""
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_stderr import check, count_writes, scan  # noqa: E402


def _make_tree(spec: dict[str, str]) -> Path:
    """Build a tmpdir tree. spec = {path relative to a crates dir: source}."""
    root = Path(tempfile.mkdtemp()) / "crates"
    for rel, body in spec.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body, encoding="utf-8")
    return root


def _one_file(body: str) -> int:
    d = Path(tempfile.mkdtemp())
    p = d / "a.rs"
    p.write_text(body, encoding="utf-8")
    return count_writes(p)


def main() -> int:
    failures: list[str] = []

    # 1. Detection: every print macro form counts, and an identifier that
    # merely contains print does not. The negative half matters because the
    # gate script itself defines a fn named print_summary -- a regex without
    # the bang would flag the tooling that reports the flags.
    n = _one_file(
        'fn f() {\n'
        '    eprintln!("a");\n'
        '    println!("b");\n'
        '    eprint!("c");\n'
        '    print!("d");\n'
        '}\n'
    )
    if n != 4:
        failures.append(f"detection: expected 4 macro forms counted, got {n}")
    n = _one_file(
        'fn print_summary() {}\n'
        'fn g() { let sprint = 1; let _ = sprint; print_summary(); }\n'
        'fn h() { eprintln!("real"); }\n'
    )
    if n != 1:
        failures.append(
            f"detection: only the real macro should count (fn names holding "
            f"print must not), got {n}"
        )

    # 2. The trailing test module is exempt, and code above it is not. Both
    # halves in one file so a cutoff of 0 (which would exempt everything)
    # and a cutoff of EOF (which would exempt nothing) both fail.
    n = _one_file(
        'fn real() { eprintln!("runtime"); }\n'
        '\n'
        '#[cfg(test)]\n'
        'mod tests {\n'
        '    #[test]\n'
        '    fn t() { println!("test output"); }\n'
        '}\n'
    )
    if n != 1:
        failures.append(
            f"cfg(test) cutoff: expected only the runtime write counted, got {n}"
        )

    # 3. A #[cfg(test)] that is not followed by a mod must NOT start the
    # cutoff, or a test-only import near the top of a file would blank every
    # real write below it. Paired: the later real test mod still cuts off.
    n = _one_file(
        '#[cfg(test)]\n'
        'use std::io::Write;\n'
        '\n'
        'fn real() { eprintln!("still counted"); }\n'
        '\n'
        '#[cfg(test)]\n'
        'mod tests {\n'
        '    fn t() { println!("exempt"); }\n'
        '}\n'
    )
    if n != 1:
        failures.append(
            f"cfg(test) on a non-mod item must not blank the file, got {n}"
        )

    # 4. Path exemptions, each against a positive control in the same tree:
    # the src file must be the ONLY reported path.
    body = 'fn f() { eprintln!("x"); }\n'
    root = _make_tree({
        "c/src/real.rs": body,
        "c/tests/it.rs": body,
        "c/examples/demo.rs": body,
        "c/benches/b.rs": body,
        "c/src/thing_tests.rs": body,
    })
    found = scan(root)
    if set(found) != {"crates/c/src/real.rs"}:
        failures.append(
            f"path exemptions: expected only crates/c/src/real.rs, got {sorted(found)}"
        )

    # 5. The three check verdicts. Exact match is silent; over-count and
    # under-count both fire. Injected tables throughout, which also pins the
    # injection contract: if check read the module globals instead of its
    # parameters, the exact-match case would report the real repo's files.
    errs = check({"p.rs": 2}, console_ok={}, baseline={"p.rs": 2})
    if errs:
        failures.append(f"exact match must pass, got {errs}")
    errs = check({"p.rs": 3}, console_ok={}, baseline={"p.rs": 2})
    if not any("3 console write(s), 2 allowed" in e for e in errs):
        failures.append(f"over-count must fire, got {errs}")
    errs = check({"p.rs": 1}, console_ok={}, baseline={"p.rs": 2})
    if not any("allow 2 console write(s), found 1" in e for e in errs):
        failures.append(f"under-count must fire, got {errs}")
    errs = check({}, console_ok={}, baseline={"gone.rs": 1})
    if not any("gone.rs" in e and "found 0" in e for e in errs):
        failures.append(f"a table entry with no writes left must fire, got {errs}")

    # 6. An unlisted file with a write fires -- the case the gate exists for.
    errs = check({"new.rs": 1}, console_ok={}, baseline={})
    if not any("new.rs" in e and "1 console write(s), 0 allowed" in e for e in errs):
        failures.append(f"an unlisted write must fire, got {errs}")

    # 7. The two tables sum for one file. This is the cli entry point's
    # shape: argument parsing (console, correct) and post-alternate-screen
    # wiring (not correct) in one module. Allowed is the sum; one more than
    # the sum still fires.
    errs = check({"m.rs": 3}, console_ok={"m.rs": 2}, baseline={"m.rs": 1})
    if errs:
        failures.append(f"console + baseline must sum for one file, got {errs}")
    errs = check({"m.rs": 4}, console_ok={"m.rs": 2}, baseline={"m.rs": 1})
    if not any("4 console write(s), 3 allowed" in e for e in errs):
        failures.append(f"one over the summed allowance must fire, got {errs}")

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[stderr-gate-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[stderr-gate-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
