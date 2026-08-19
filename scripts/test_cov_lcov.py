#!/usr/bin/env python3
"""Regression tests for the shared lcov stale-mapping detector (cov_lcov).

Every coverage verdict in the repo rides on this one module -- diff-cov
(check_diff_coverage.py) and the workspace total (check_coverage.sh)
both take their truth from it. A future edit that weakens the detector
(normalize() doing a prefix match instead of find, or the boundary going
from > to >=) would reopen the stale-mapping gap silently, and the gate is
the machine backstop for a rule the model applies unreliably, so the gate
itself must be guarded.

Pins the four branches the detector was hand-verified on, paired so each
reject case carries a must-pass counterpart:
  - clean lcov (last line of the file) -> exit 0, no false positive
  - past-EOF line -> exit 2 + evidence naming the file + line
  - missing lcov file -> exit 1 + diagnostic
  - empty/unparseable lcov -> exit 1 + diagnostic

The clean case deliberately sits on the last line of the file rather than
somewhere in the middle. An off-by-one is the likeliest way this detector
breaks, and a middle line cannot see one: an in-range assertion far from the
boundary passes under either comparison. Verified by mutation -- with the
clean line at 1 the > to >= rewrite went undetected, with it on the last
line the rewrite fails this test.

Run: python3 scripts/test_cov_lcov.py  (wired into make check as
cov-lcov-tests). Exit 0 = pass, 1 = fail.
"""
import contextlib
import io
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from cov_lcov import check, normalize, stale_mapping_evidence  # noqa: E402

# A real source file the detector can stat + read. normalize() maps any path
# form to crates/... so this is repo-root-relative.
ROOT = Path(__file__).resolve().parent.parent
REAL_SRC = "crates/houyicoder-sandbox/src/lib.rs"


def _real_line_count() -> int:
    return len((ROOT / REAL_SRC).read_text(encoding="utf-8").splitlines())


def _write_lcov(text: str) -> Path:
    tf = tempfile.NamedTemporaryFile(
        mode="w", suffix=".lcov", delete=False, encoding="utf-8")
    tf.write(text)
    tf.close()
    return Path(tf.name)


def _run(lcov_path: Path) -> tuple[int, str]:
    """Call check() with stderr captured so a passing test does not print the
    detector's reject text (which would read like a failure to a casual
    reader). Returns (exit_code, stderr_text)."""
    buf = io.StringIO()
    with contextlib.redirect_stderr(buf):
        rc = check(lcov_path)
    return rc, buf.getvalue()


def main() -> int:
    failures = []
    last = _real_line_count()
    past = last + 50

    # 1. clean lcov -> exit 0, no false positive. Proved first so a weakening
    #    that invents evidence is caught before any reject case. The line is
    #    the file's last, which is in range and one past it is not, so an
    #    off-by-one in the comparison fails here.
    p = _write_lcov(f"SF:{REAL_SRC}\nDA:{last},1\nend_of_record\n")
    rc, err = _run(p)
    p.unlink(missing_ok=True)
    if rc != 0:
        failures.append(f"clean lcov: expected exit 0, got {rc} ({err.strip()})")
    if err.strip():
        failures.append(f"clean lcov: expected no stderr, got: {err.strip()}")

    # 2. past-EOF line -> exit 2 + evidence naming the file + the line.
    p = _write_lcov(f"SF:{REAL_SRC}\nDA:{past},0\nend_of_record\n")
    rc, err = _run(p)
    p.unlink(missing_ok=True)
    if rc != 2:
        failures.append(f"stale lcov: expected exit 2, got {rc} ({err.strip()})")
    if REAL_SRC not in err:
        failures.append(f"stale lcov: evidence missing file {REAL_SRC}: {err.strip()}")
    if str(past) not in err:
        failures.append(f"stale lcov: evidence missing line {past}: {err.strip()}")

    # evidence() directly surfaces the offending file + line, paired with the
    # clean case (no evidence when the line is in range).
    exe_stale = {REAL_SRC: {past: False}}
    ev = stale_mapping_evidence(exe_stale)
    if not ev or REAL_SRC not in ev[0] or str(past) not in ev[0]:
        failures.append(f"evidence() did not surface file+line: {ev}")
    exe_clean = {REAL_SRC: {last: True}}
    if stale_mapping_evidence(exe_clean):
        failures.append("evidence() invented evidence for an in-range line")

    # 3. missing file -> exit 1 + diagnostic.
    missing = Path(tempfile.gettempdir()) / "nonexistent_cov_lcov_test.lcov"
    rc, err = _run(missing)
    if rc != 1:
        failures.append(f"missing lcov: expected exit 1, got {rc}")
    if "not found" not in err:
        failures.append(f"missing lcov: diagnostic missing 'not found': {err.strip()}")

    # 4. empty/unparseable lcov -> exit 1 + diagnostic (no executable lines).
    p = _write_lcov("")
    rc, err = _run(p)
    p.unlink(missing_ok=True)
    if rc != 1:
        failures.append(f"empty lcov: expected exit 1, got {rc}")
    if "no executable lines" not in err:
        failures.append(f"empty lcov: diagnostic missing 'no executable lines': {err.strip()}")

    # 5. normalize() maps any path form to crates/... so evidence points at the
    #    real file. Pin it so a prefix-match rewrite cannot quietly break it.
    if normalize("abs/path/to/crates/foo/lib.rs") != "crates/foo/lib.rs":
        failures.append("normalize did not strip to crates/ prefix")

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[cov-lcov-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[cov-lcov-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
