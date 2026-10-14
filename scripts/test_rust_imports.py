#!/usr/bin/env python3
"""Regression tests for the import-locality rule and its ratchet.

Locks the detector's positive and negative cases (a module-scope use is not
a finding, a use in a nested block inside a function is) and both directions
of the ratchet's pure verdict. The shared runner prints one verdict line, so
a case name is the failure label rather than its own output.
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


def test_module_scope_use_is_allowed():
    assert len(list(inline_use_violations(MODULE_SCOPE))) == 0


def test_use_in_a_function_is_a_finding():
    assert len(list(inline_use_violations(IN_FN))) == 1


def test_use_in_a_nested_block_is_a_finding():
    assert len(list(inline_use_violations(IN_NESTED))) == 1


def test_brace_in_a_string_does_not_hide_the_body():
    assert len(list(inline_use_violations(BRACE_IN_STRING))) == 1


def test_pub_crate_fn_body_is_seen():
    assert len(list(inline_use_violations(PUB_CRATE_FN))) == 1


def test_pub_unsafe_fn_body_is_seen():
    assert len(list(inline_use_violations(PUB_UNSAFE_FN))) == 1


def test_body_opening_after_a_multiline_signature_is_seen():
    assert len(list(inline_use_violations(MULTILINE_SIG))) == 1


def test_a_lifetime_is_not_read_as_a_char_literal():
    assert len(list(inline_use_violations(LIFETIME_SIG))) == 1


def test_a_one_line_body_is_seen():
    assert len(list(inline_use_violations(ONE_LINE_FN))) == 1


def test_a_char_literal_before_the_use_does_not_hide_it():
    assert len(list(inline_use_violations(CHAR_THEN_USE))) == 1


def test_two_variants_of_one_type_group_into_one_import():
    assert repeated_deep_paths(PATH_TWICE) == {"crate::state::Screen": 2}


def test_module_type_variant_repeated_is_caught():
    assert repeated_deep_paths(MOD_TYPE_VARIANT_TWICE) == {"mpsc::TryRecvError": 2}


def test_a_deep_path_used_once_is_a_finding():
    assert repeated_deep_paths(PATH_ONCE) == {"crate::state::Screen": 1}


def test_a_name_the_file_defines_is_left_qualified():
    assert repeated_deep_paths(PATH_ONCE + "fn Screen() {}\n") == {}


def test_a_name_two_chains_share_is_left_qualified():
    two = "fn a() { let _ = crate::one::Widget::new(); let _ = crate::two::Widget::new(); }\n"
    assert repeated_deep_paths(two) == {}


def test_a_name_already_imported_from_another_chain_is_left_qualified():
    imported_and_other = "use crate::one::Widget;\nfn a() { let _ = Widget::new(); let _ = crate::two::Widget::new(); }\n"
    assert repeated_deep_paths(imported_and_other) == {}


def test_a_chain_a_glob_already_covers_is_left_qualified():
    globbed = "pub use run::*;\nfn a() { let _ = crate::frontend::run::Widget::new(); }\n"
    assert repeated_deep_paths(globbed) == {}


def test_a_local_binding_of_the_same_name_keeps_the_chain_qualified():
    shadowed = "fn a() { let id = 7; let _ = std::process::id(); let _ = id; }\n"
    assert repeated_deep_paths(shadowed) == {}


def test_a_prelude_name_is_left_qualified():
    # Importing std::fmt::Result would hide the prelude Result<T, E> the
    # signatures use, so the qualified spelling is correct here.
    assert repeated_deep_paths("fn a() -> std::fmt::Result { Ok(()) }\n") == {}


def test_a_name_outside_the_prelude_is_still_a_finding():
    # std::fmt::Formatter is not in the prelude; importing it is legal, so
    # the qualified spelling is a finding like any other.
    assert repeated_deep_paths("fn a() -> std::fmt::Formatter { todo!() }\n") == {
        "std::fmt::Formatter": 1
    }


def test_a_four_segment_deep_path_is_caught_even_once():
    assert repeated_deep_paths(DEEP_FOUR_ONCE) == {
        "houyicoder_protocol::frontend::permission::PermissionMode": 1
    }


def test_one_separator_is_allowed():
    assert repeated_deep_paths(ONE_SEPARATOR) == {}


def test_ratchet_holds_at_the_baseline():
    assert evaluate(["x"], ["y"], 1, 1) == 0


def test_ratchet_blocks_growth():
    assert evaluate(["x", "y"], ["y"], 1, 1) == 1


def test_ratchet_blocks_growth_of_paths():
    assert evaluate(["x"], ["y", "z"], 1, 1) == 1


def test_the_hook_refuses_a_write_with_a_local_import():
    assert _hook_verdict(IN_FN) == 2


def test_the_hook_refuses_a_write_with_a_deep_path():
    assert _hook_verdict(DEEP_FOUR_ONCE) == 2


def test_the_hook_allows_a_write_with_a_module_scope_import():
    assert _hook_verdict(MODULE_SCOPE) == 0


def test_the_hook_counts_a_repeated_pair_once():
    assert evaluate(["x"], ["p", "q"], 1, 1) == 1


if __name__ == "__main__":
    from test_runner import run

    sys.exit(run("rust-imports", dict(globals())))
