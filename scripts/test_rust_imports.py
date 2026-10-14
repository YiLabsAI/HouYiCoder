#!/usr/bin/env python3
"""Regression tests for the import-locality rule and its ratchet.

Locks the detector's positive and negative cases (a module-scope use is not
a finding, a use in a nested block inside a function is) and both directions
of the ratchet's pure verdict.
"""
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_rust_imports import evaluate  # noqa: E402
from rules.imports import inline_use_violations, repeated_deep_paths  # noqa: E402

MODULE_SCOPE = 'mod tests {\n    use crate::state::App;\n    #[test]\n    fn t() {}\n}\n'
IN_FN = 'fn f() {\n    use crate::state::App;\n    let _ = App;\n}\n'
IN_NESTED = 'fn f() {\n    if ready {\n        use crate::state::App;\n    }\n}\n'
BRACE_IN_STRING = 'fn f() {\n    let s = "{";\n    use crate::state::App;\n}\n'
PUB_CRATE_FN = 'pub(crate) fn f() {\n    use crate::state::App;\n}\n'
PUB_UNSAFE_FN = 'pub unsafe fn f() {\n    use crate::state::App;\n}\n'
MULTILINE_SIG = 'fn f(\n    x: u8,\n) -> u8 {\n    use crate::state::App;\n    0\n}\n'
LIFETIME_SIG = "fn f<'a>(x: &'a str) -> &'a str {\n    use crate::state::App;\n    x\n}\n"
ONE_LINE_FN = 'fn f() { use crate::state::App; }\n'
CHAR_THEN_USE = "fn f() {\n    let c = 'x';\n    use crate::state::App;\n}\n"
PATH_TWICE = 'fn a() { let x = crate::state::Screen::Working; }\nfn b() { let y = crate::state::Screen::Focus; }\n'
MOD_TYPE_VARIANT_TWICE = 'fn a() { let x = mpsc::TryRecvError::Empty; }\nfn b() { let y = mpsc::TryRecvError::Disconnected; }\n'
PATH_ONCE = 'fn a() { let x = crate::state::Screen::Working; }\n'
DEEP_FOUR_ONCE = 'fn a() { let x = houyicoder_protocol::frontend::permission::PermissionMode::Manual; }\n'
ONE_SEPARATOR = 'fn a() { let x = Screen::Working; let y = Screen::Focus; }\n'


def check(name, got, want):
    if got != want:
        print(f"FAIL {name}: got {got}, want {want}")
        return False
    print(f"ok  {name}")
    return True


def _hook_verdict(new_string):
    """Run the write-time hook on one Edit payload and report its exit code."""
    payload = json.dumps(
        {
            "tool_name": "Edit",
            "tool_input": {"file_path": "crates/x/src/y.rs", "new_string": new_string},
        }
    )
    proc = subprocess.run(
        [sys.executable, str(Path(__file__).resolve().parent / "hook_rust.py")],
        input=payload,
        capture_output=True,
        text=True,
    )
    return proc.returncode


def main() -> int:
    ok = True
    ok &= check("module scope use is allowed", len(list(inline_use_violations(MODULE_SCOPE))), 0)
    ok &= check("use in a function is a finding", len(list(inline_use_violations(IN_FN))), 1)
    ok &= check("use in a nested block is a finding", len(list(inline_use_violations(IN_NESTED))), 1)
    ok &= check("a brace inside a string does not hide the body", len(list(inline_use_violations(BRACE_IN_STRING))), 1)
    ok &= check("a pub(crate) fn body is seen", len(list(inline_use_violations(PUB_CRATE_FN))), 1)
    ok &= check("a pub unsafe fn body is seen", len(list(inline_use_violations(PUB_UNSAFE_FN))), 1)
    ok &= check("a body opening after a multi-line signature is seen", len(list(inline_use_violations(MULTILINE_SIG))), 1)
    ok &= check("a lifetime is not read as a char literal", len(list(inline_use_violations(LIFETIME_SIG))), 1)
    ok &= check("a one-line body is seen", len(list(inline_use_violations(ONE_LINE_FN))), 1)
    ok &= check("a char literal before the use does not hide it", len(list(inline_use_violations(CHAR_THEN_USE))), 1)
    ok &= check(
        "two variants of one type group into one import",
        repeated_deep_paths(PATH_TWICE),
        {"crate::state::Screen": 2},
    )
    ok &= check(
        "module::Type::Variant repeated is caught so Type is imported",
        repeated_deep_paths(MOD_TYPE_VARIANT_TWICE),
        {"mpsc::TryRecvError": 2},
    )
    ok &= check("a path used once is allowed", repeated_deep_paths(PATH_ONCE), {})
    ok &= check(
        "a 4-segment deep path is caught even once",
        repeated_deep_paths(DEEP_FOUR_ONCE),
        {"houyicoder_protocol::frontend::permission::PermissionMode": 1},
    )
    ok &= check("one separator is allowed", repeated_deep_paths(ONE_SEPARATOR), {})
    ok &= check("ratchet holds at the baseline", evaluate(["x"], ["y"], 1, 1), 0)
    ok &= check("ratchet blocks growth", evaluate(["x", "y"], ["y"], 1, 1), 1)
    ok &= check("ratchet blocks growth of paths", evaluate(["x"], ["y", "z"], 1, 1), 1)
    ok &= check("the hook refuses a write with a local import", _hook_verdict(IN_FN), 2)
    ok &= check("the hook refuses a write with a deep path", _hook_verdict(DEEP_FOUR_ONCE), 2)
    ok &= check("the hook allows a write with a module-scope import", _hook_verdict(MODULE_SCOPE), 0)
    ok &= check(
        "the hook counts a repeated pair once, not once per use",
        evaluate(["x"], ["p", "q"], 1, 1),
        1,
    )
    if not ok:
        print("FAIL test_rust_imports")
        return 1
    print("PASS test_rust_imports")
    return 0


if __name__ == "__main__":
    sys.exit(main())
