#!/usr/bin/env python3
"""Regression tests for the cross-target compile ratchet.

The counting rule and the baseline verdict are pure functions over captured
output, so they are pinned here without a cross-compile: a rustc diagnostic
counts, a dependency's build-script failure does not (it is a toolchain
limit, not a finding).
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_cross_target import _BUILD_SCRIPT_RE, _ERROR_RE, error_count, gated_crates  # noqa: E402

RUSTC_ERROR = """error[E0425]: cannot find type `Mutex` in this scope
  --> crates/houyicoder-tui/src/selection/linux.rs:15:20
"""
TWO_RUSTC_ERRORS = RUSTC_ERROR + RUSTC_ERROR
BUILD_SCRIPT_FAILURE = """error: failed to run custom build command for `ring v0.17.14`
Caused by:
  process didn't exit successfully: build-script-build
"""


def test_a_rustc_diagnostic_is_counted():
    assert len(_ERROR_RE.findall(RUSTC_ERROR)) == 1
    assert len(_ERROR_RE.findall(TWO_RUSTC_ERRORS)) == 2


def test_a_build_script_failure_is_not_a_rustc_diagnostic():
    assert _BUILD_SCRIPT_RE.search(BUILD_SCRIPT_FAILURE)
    assert len(_ERROR_RE.findall(BUILD_SCRIPT_FAILURE)) == 0


def test_a_plain_error_without_a_code_is_not_counted():
    # A build-script line and a summary line both start with "error"; only a
    # coded rustc diagnostic is a finding.
    assert len(_ERROR_RE.findall("error: could not compile `x`\n")) == 0


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
        assert error_count("example", "x86_64-unknown-linux-gnu", Path(".")) == 2


if __name__ == "__main__":
    from test_runner import run

    sys.exit(run("cross-target", dict(globals())))
