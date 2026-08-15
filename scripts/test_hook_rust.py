#!/usr/bin/env python3
"""Regression tests for the comment/commit codename gate (rules.comments.CODENAME
and the patterns check_rs_comments / commit_msg_lint reuse from it).

Pins the single-digit sprint/task gap fix: S1..S9 and T1..T9 used to slip
past the S/T quantifier that required two-plus digits, so a commit body or
.rs comment referencing "S7" landed unchallenged. The tightened quantifier
(one-plus digits) catches them. This test prevents a future widening of the
quantifier from re-opening the gap silently — the gate is the machine
backstop for a rule the model applies unreliably post-compact, so the gate
itself must be guarded.

Run: python3 scripts/test_hook_rust.py  (wired into make check as gate-tests).
Exit 0 = pass, 1 = fail.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from commit_msg_lint import _ACCEPTANCE_CODES  # noqa: E402
from rules.comments import CODENAME, COMPARISON, OWN_NAME  # noqa: E402


def _caught(text: str) -> bool:
    return CODENAME.search(text) is not None


def _caught_acceptance(text: str) -> bool:
    return _ACCEPTANCE_CODES.search(text) is not None


def _caught_comparison(text: str) -> bool:
    return COMPARISON.search(text) is not None


def main() -> int:
    failures = []

    def must_flag(label, text):
        if not _caught(text):
            failures.append(f"expected flag, missed: {label!r} in {text!r}")

    def must_pass(label, text):
        if _caught(text):
            m = CODENAME.search(text)
            failures.append(
                f"unexpected flag of {label!r} (matched {m.group(0)!r}) in {text!r}"
            )

    # Single-digit sprint/task refs — the gap fix. These slipped before.
    must_flag("S7 in parens", "beforeCompact (S7) will reuse the scan")
    must_flag("bare S1", "guards the S1 wiring")
    must_flag("bare T4", "pinned by T4")
    must_flag("S99-T99 form", "see S99-T99 for the surfaced contract")

    # Multi-digit refs — already caught, stay caught.
    must_flag("S26", "guards the S26 scaffold")
    must_flag("D8", "the D8 stale doc batch")
    must_flag("A3", "the A3 memory fix")
    must_flag("F1", "the F1 timeout")
    must_flag("SEC-3", "the SEC-3 gate")
    must_flag("stage2", "landed in stage2")
    must_flag("B1-04", "the B1-04 family")
    must_flag("Bet A.1", "Bet A.1 form")
    must_flag("section §5.8", "see §5.8 of the design")

    # Negatives — legitimate prose that must NOT trip.
    must_pass("step number", "step 1 of the flow")
    must_pass("version 2", "version 2 of the protocol")
    must_pass("HTTP/2", "speak HTTP/2")
    must_pass("no digit after S/T", "the Storage backend")
    must_pass("mid-word S", "the Statistics collector")
    must_pass("plural word", "tests")

    # Own-name ban (separate pattern, same gate family).
    if not OWN_NAME.search("// uses houyicoder-api"):
        failures.append("own-name pattern missed houyicoder-api")

    # Comparison-framing arm — a comment must describe this design, not
    # measure it against another implementation. The euphemism forms are
    # worse than naming a product outright (a reader cannot resolve them),
    # so the gate is the machine backstop for a rule the model applies
    # unreliably. Pin both directions so a future widening cannot re-open
    # the gap silently.
    cmp_flag = [
        ("the reference agent", "the reference agent increments turnCount"),
        ("the reference possessive", "mirrors the reference's taxonomy"),
        ("a reference vendor", "an OpenAI-compatible, a reference vendor, or stub"),
        ("a reference agent SDK", "per a reference agent SDK"),
        ("surpasses the reference", "this surpasses the reference on recall"),
        ("bare surpass point", "this is the surpass point"),
        ("bare surpass surface", "a surpass surface: the linked-worktree manager"),
        ("surpassing it", "surpassing it on three axes"),
        ("surpass annotation", "surpass: no libc dependency"),
        ("the canonical layout", "matches the canonical layout end-to-end"),
        ("a prior project", "unlike a prior project, this crate"),
    ]
    for label, text in cmp_flag:
        if not _caught_comparison(text):
            failures.append(f"expected comparison flag, missed: {label!r} in {text!r}")
    cmp_pass = [
        ("reference implementation", "the reference implementation is generic"),
        ("a reference to X", "a reference to the config layer"),
        ("as a reference", "use the manifest as a reference"),
        ("the referenced block", "the referenced block stays in the CAS"),
        ("reference material", "reference material pulled from external sources"),
        ("mirrors the frontend's", "mirrors the frontend's former always_allow_rule"),
        ("the canonical shape", "the canonical shape of the wire enum"),
        ("canonicalize the path", "canonicalize the path before the probe"),
    ]
    for label, text in cmp_pass:
        if _caught_comparison(text):
            m = COMPARISON.search(text)
            failures.append(
                f"unexpected comparison flag of {label!r} (matched {m.group(0)!r}) in {text!r}"
            )

    # Acceptance/charter internal codes (commit-specific pattern). These are
    # the phase-gate ids, journey ids, hazard ids, and charter version stamps
    # that slipped past CODENAME (D-codes were already caught, but G/U/H/v
    # were not). The author kept leaking them into commit prose, so the gate
    # was extended; pin the extension so a future widening cannot re-open it.
    acc_flag = [
        ("phase gate G0", "reconcile G0 to G5"),
        ("journey U7", "the U7 failure-reflection loop"),
        ("hazard H11", "defer the H11 seatbelt fix"),
        ("charter v2.4", "the v2.4 dimension"),
    ]
    for label, text in acc_flag:
        if not _caught_acceptance(text):
            failures.append(f"expected acceptance-code flag, missed: {label!r} in {text!r}")
    # Legitimate prose must NOT trip the acceptance pattern. "version 2"
    # lacks the v\d.\d form; "H264 codec" is not in this repo's domain but
    # pinned here so a future tightening that catches it is a deliberate call.
    acc_pass = [
        ("version 2 bare", "version 2 of the protocol"),
        ("step 1", "step 1 of the flow"),
    ]
    for label, text in acc_pass:
        if _caught_acceptance(text):
            m = _ACCEPTANCE_CODES.search(text)
            failures.append(
                f"unexpected acceptance-code flag of {label!r} (matched {m.group(0)!r}) in {text!r}"
            )

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[gate-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[gate-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
