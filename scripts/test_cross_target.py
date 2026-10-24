#!/usr/bin/env python3
"""Regression tests for the cross-target compile ratchet.

The counting rule and the baseline verdict are pure functions over captured
output, so they are pinned here without a cross-compile. Three properties are
what make the gate able to see a break at all:

A warning counts, because CI builds with warnings denied and an import that
loses its only user on the other platform is exactly that warning. A
dependency's build-script failure does not count, because the run stops before
this crate is compiled and every diagnostic after it belongs to the toolchain.
A run-level summary does not count, because it would multiply the total by the
number of failures.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_cross_target import (  # noqa: E402
    _BUILD_SCRIPT_RE,
    _DIAG_RE,
    crate_error_count,
    gated_crates,
)

RUSTC_ERROR = """error[E0425]: cannot find type `Mutex` in this scope
  --> crates/houyicoder-tui/src/selection/linux.rs:15:20
"""
TWO_RUSTC_ERRORS = RUSTC_ERROR + RUSTC_ERROR
UNUSED_IMPORT_WARNING = """warning: unused import: `houyicoder_api::launcher::SpawnRequest`
  --> crates/houyicoder-core/src/agent/tools/webfetch/route.rs:22:5
"""
BUILD_SCRIPT_FAILURE = """warning: ring@0.17.14: Compiler family detection failed
error: failed to run custom build command for `ring v0.17.14`
warning: build failed, waiting for other jobs to finish...
Caused by:
  process didn't exit successfully: build-script-build
"""


def test_a_rustc_diagnostic_is_counted():
    assert len(_DIAG_RE.findall(RUSTC_ERROR)) == 1
    assert len(_DIAG_RE.findall(TWO_RUSTC_ERRORS)) == 2


def test_an_unused_import_warning_is_counted():
    # CI denies warnings, so this is the failure that actually reaches the
    # other platforms. A gate that counted only coded errors saw nothing.
    assert len(_DIAG_RE.findall(UNUSED_IMPORT_WARNING)) == 1


def test_a_build_script_failure_is_not_a_rustc_diagnostic():
    assert _BUILD_SCRIPT_RE.search(BUILD_SCRIPT_FAILURE)
    assert len(_DIAG_RE.findall(BUILD_SCRIPT_FAILURE)) == 0


def test_a_plain_error_without_a_code_is_not_counted():
    # A build-script line and a summary line both start with "error"; neither
    # is a finding of its own.
    assert len(_DIAG_RE.findall("error: could not compile `x`\n")) == 0
    assert len(_DIAG_RE.findall("error: aborting due to 3 previous errors\n")) == 0
    assert len(_DIAG_RE.findall("warning: 3 warnings emitted\n")) == 0


def test_a_skipped_build_script_reports_no_count():
    import subprocess
    from unittest.mock import patch

    with patch(
        "check_cross_target.subprocess.run",
        return_value=subprocess.CompletedProcess([], 101, "", BUILD_SCRIPT_FAILURE),
    ):
        assert crate_error_count("example", "x86_64-unknown-linux-gnu", Path(".")) is None


def test_gated_crates_finds_a_cfg_gated_source(tmp_path=None):
    import tempfile

    root = Path(tempfile.mkdtemp())
    (root / "crates" / "plain" / "src").mkdir(parents=True)
    (root / "crates" / "plain" / "Cargo.toml").write_text("[package]\n")
    (root / "crates" / "plain" / "src" / "lib.rs").write_text("pub fn a() {}\n")
    (root / "crates" / "gated" / "src").mkdir(parents=True)
    (root / "crates" / "gated" / "Cargo.toml").write_text("[package]\n")
    (root / "crates" / "gated" / "src" / "lib.rs").write_text(
        '#[cfg(target_os = "linux")]\npub fn b() {}\n'
    )
    assert gated_crates(root) == ["gated"]


def test_nonzero_exit_without_error_code_still_counts():
    import subprocess
    from unittest.mock import patch

    with patch(
        "check_cross_target.subprocess.run",
        return_value=subprocess.CompletedProcess([], 101, "", "error: unused import: `Foo`\n"),
    ):
        assert crate_error_count("example", "x86_64-unknown-linux-gnu", Path(".")) == 2


if __name__ == "__main__":
    from test_runner import run

    sys.exit(run("cross-target", dict(globals())))
