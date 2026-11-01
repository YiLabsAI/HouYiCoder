#!/usr/bin/env python3
"""Regression test for the turn-dispatch measurement.

The numbers are only useful if each count means what the runtime rule means
by the same words. These tests build small event streams and check the
counts directly. They also state the family membership the rule's predicates
carry today: extending a family means updating the expectation in the same
change, so someone confirms the new name really belongs to that family
before the measurement starts counting it.
"""

import contextlib
import io
import json
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import measure_turn_dispatch as metrics

FACTS = metrics.RuleFacts(
    writes=frozenset({"edit", "write"}),
    shells=frozenset({"bash", "sh"}),
    failure_words=frozenset({"failed", "error", "panic", "assertion"}),
    skip_prefix="skipped:",
)


def turn(number: int = 0) -> dict:
    """One turn boundary event."""
    return {"type": "TurnStarted", "turn": number, "call_in_turn": 0}


def call(call_id: str, tool: str, input_value=None) -> dict:
    """One tool call event."""
    return {
        "type": "ToolCall",
        "call_id": call_id,
        "tool": tool,
        "input": {} if input_value is None else input_value,
    }


def answer(call_id: str, output) -> dict:
    """One tool result event."""
    return {"type": "ToolResult", "call_id": call_id, "output": output}


def asked(call_id: str, tool: str = "bash") -> dict:
    """One approval-ask event."""
    return {"type": "PermissionAsked", "call_id": call_id, "tool": tool}


def decided(call_id: str, verdict: str = "Approved", tool: str = "bash") -> dict:
    """One permission verdict event."""
    return {
        "type": "PermissionDecision",
        "call_id": call_id,
        "tool": tool,
        "verdict": verdict,
        "scope": "once",
    }


def measured(events: list[dict]) -> dict:
    """Measure a constructed event stream."""
    return metrics.measure(metrics.group_turns(events), FACTS)


def edit_with(output) -> dict:
    """Measure an edit answered with output and followed by one shell call."""
    return measured(
        [
            turn(),
            call("e1", "edit"),
            answer("e1", output),
            call("b1", "bash"),
        ]
    )


def test_rule_literals_follow_the_code() -> None:
    facts = metrics.load_rule_facts()
    assert facts.writes == frozenset(
        {"write", "edit", "multiedit", "patch", "str_replace"}
    ), f"writes: {sorted(facts.writes)}"
    assert facts.shells == frozenset({"bash", "sh", "exec", "shell"}), (
        f"shells: {sorted(facts.shells)}"
    )
    assert facts.failure_words == frozenset(
        {"failed", "error", "panic", "assertion"}
    ), f"failure words: {sorted(facts.failure_words)}"
    assert facts.skip_prefix == "skipped:", facts.skip_prefix


def test_missing_predicate_names_it() -> None:
    try:
        metrics.literals_in("pub fn other() {}", "writes_files")
    except metrics.FamilySourceError as error:
        assert str(error).startswith("no function named writes_files"), error
        return
    raise AssertionError("a missing predicate must raise FamilySourceError")


def test_reworded_skip_marker_is_rejected() -> None:
    source = 'const SKIPPED_AFTER_EDIT: &str = "held: the edit did not finish";'
    try:
        metrics.skip_prefix(source)
    except metrics.FamilySourceError:
        return
    raise AssertionError("a marker without the skip prefix must be rejected")


def test_turn_boundaries_group_calls() -> None:
    events = [
        turn(),
        call("c1", "edit"),
        call("c2", "bash"),
        turn(),
        call("c3", "bash"),
    ]
    log = metrics.group_turns(events)
    assert len(log.turns) == 2, len(log.turns)
    assert [len(item.calls) for item in log.turns] == [2, 1]
    assert [item.turn for item in log.calls] == [0, 0, 1]


def test_legacy_log_groups_on_user_input() -> None:
    events = [
        {"type": "UserInput", "text": "go"},
        call("c1", "bash"),
        {"type": "UserInput", "text": "again"},
        call("c2", "bash"),
    ]
    log = metrics.group_turns(events)
    assert len(log.turns) == 2, len(log.turns)
    assert [item.turn for item in log.calls] == [0, 1]


def test_legacy_prefix_keeps_its_turns() -> None:
    events = [
        {"type": "UserInput", "text": "go"},
        call("c1", "bash"),
        {"type": "UserInput", "text": "again"},
        call("c2", "bash"),
        turn(),
        call("c3", "bash"),
    ]
    log = metrics.group_turns(events)
    assert [item.turn for item in log.calls] == [0, 1, 2], [
        item.turn for item in log.calls
    ]


def test_each_user_input_opens_a_turn() -> None:
    log = metrics.group_turns(
        [
            {"type": "UserInput", "text": "go"},
            call("e1", "edit"),
            {"type": "UserInput", "text": "still"},
            {"type": "UserInput", "text": "more"},
            call("b1", "bash"),
        ]
    )
    assert [item.turn for item in log.calls] == [0, 2], [
        item.turn for item in log.calls
    ]
    assert metrics.measure(log, FACTS)["split_turns"] == 0, (
        "a check two user turns later is not a next-turn split"
    )


def test_empty_model_turn_stays_a_turn() -> None:
    log = metrics.group_turns([turn(), turn(), call("c1", "bash")])
    assert len(log.turns) == 2, len(log.turns)


def test_leading_input_shares_the_first_turn() -> None:
    log = metrics.group_turns(
        [{"type": "UserInput", "text": "go"}, turn(), call("c1", "bash")]
    )
    assert len(log.turns) == 1, len(log.turns)


def test_tool_written_input_does_not_split_a_turn() -> None:
    log = metrics.group_turns(
        [
            turn(),
            call("c1", "bash"),
            {"type": "UserInput", "text": "recalled"},
            call("c2", "bash"),
        ]
    )
    assert len(log.turns) == 1, len(log.turns)


def test_answer_before_its_call_is_attached() -> None:
    log = metrics.group_turns(
        [turn(), answer("e1", {"success": True}), call("e1", "edit")]
    )
    assert log.calls[0].result == {"success": True}, log.calls[0].result
    assert log.unmatched == 1, log.unmatched
    assert metrics.measure(log, FACTS)["pending_edit_calls"] == 0, (
        "an answer recorded before its call still answers it"
    )


def test_co_dispatch_needs_the_edit_first() -> None:
    edit_then_shell = measured([turn(), call("c1", "edit"), call("c2", "bash")])
    assert edit_then_shell["co_dispatch_turns"] == 1
    assert edit_then_shell["edit_turns"] == 1
    shell_then_edit = measured([turn(), call("c1", "bash"), call("c2", "edit")])
    assert shell_then_edit["co_dispatch_turns"] == 0
    assert shell_then_edit["edit_turns"] == 1


def test_split_counts_the_following_turn() -> None:
    counts = measured([turn(), call("c1", "edit"), turn(), call("c2", "bash")])
    assert counts["split_turns"] == 1, counts["split_turns"]
    last_turn = measured([turn(), call("c1", "edit")])
    assert last_turn["split_turns"] == 0
    dispatched = measured(
        [
            turn(),
            call("c1", "edit"),
            call("c2", "bash"),
            turn(),
            call("c3", "bash"),
        ]
    )
    assert dispatched["split_turns"] == 0, "a co-dispatched turn is not a split"


def test_failed_anchor_counts_checks_in_both_turns() -> None:
    events = [
        turn(),
        call("e1", "edit"),
        answer("e1", {"error": "disk full"}),
        call("b1", "bash"),
        call("b2", "bash"),
        turn(),
        call("b3", "bash"),
        turn(),
        call("b4", "bash"),
    ]
    counts = measured(events)
    assert counts["failed_edit_calls"] == 1, counts["failed_edit_calls"]
    assert counts["checks_after_failed_edit"] == 2, counts["checks_after_failed_edit"]
    assert counts["shells_next_turn_after_failed_edit"] == 1
    assert counts["checks_after_missed_edit"] == 2
    assert counts["shells_next_turn_after_missed_edit"] == 1


def test_missed_and_pending_are_separate() -> None:
    refused = edit_with({"not_run": True, "refused": {"by": "user"}})
    assert refused["failed_edit_calls"] == 0
    assert refused["not_run_edit_calls"] == 1
    assert refused["missed_edit_calls"] == 1
    assert refused["checks_after_failed_edit"] == 0
    assert refused["checks_after_missed_edit"] == 1
    assert refused["checks_after_pending_edit"] == 0

    pending = measured([turn(), call("e1", "edit"), call("b1", "bash")])
    assert pending["missed_edit_calls"] == 0
    assert pending["pending_edit_calls"] == 1
    assert pending["checks_after_missed_edit"] == 0
    assert pending["checks_after_pending_edit"] == 1


def test_failure_reading_matches_the_runtime() -> None:
    empty_error = edit_with({"error": ""})
    assert empty_error["failed_edit_calls"] == 1, "an empty error string is a failure"
    benign = edit_with({"success": False, "stdout": "all good"})
    assert benign["failed_edit_calls"] == 0, "a false success needs a failure clue"
    assert benign["missed_edit_calls"] == 0
    stderr_clue = edit_with({"success": False, "stderr": "boom\ndetails"})
    assert stderr_clue["failed_edit_calls"] == 1
    stdout_clue = edit_with({"success": False, "stdout": "ok\nbuild failed"})
    assert stdout_clue["failed_edit_calls"] == 1


def test_successful_edit_does_not_anchor_or_release() -> None:
    clean = edit_with({"success": True})
    assert clean["checks_after_missed_edit"] == 0
    recovered = measured(
        [
            turn(),
            call("e1", "edit"),
            answer("e1", {"error": "disk full"}),
            call("b1", "bash"),
            call("e2", "edit"),
            answer("e2", {"success": True}),
            call("b2", "bash"),
        ]
    )
    assert recovered["checks_after_missed_edit"] == 2, (
        "a later successful edit must not release the earlier anchor"
    )


def test_withheld_checks_use_the_skip_prefix() -> None:
    events = [
        turn(),
        call("b1", "bash"),
        answer("b1", {"error": "skipped: the edit did not finish", "not_run": True}),
        call("b2", "bash"),
        answer("b2", {"error": "boom"}),
        call("b3", "bash"),
        answer("b3", {"stdout": "ok", "success": True}),
    ]
    counts = measured(events)
    assert counts["withheld_checks"] == 1, counts["withheld_checks"]


def test_approval_resend_needs_a_withheld_check() -> None:
    resent = measured(
        [
            turn(),
            call("e1", "edit"),
            asked("e1", "edit"),
            decided("e1", "Approved", "edit"),
            call("b1", "bash", {"command": "cargo test"}),
            answer(
                "b1",
                {"error": "skipped: the edit did not finish", "not_run": True},
            ),
            answer("e1", {"success": True}),
            turn(),
            call("b2", "bash", {"command": "cargo test"}),
        ]
    )
    assert resent["withheld_checks"] == 1
    assert resent["approval_resends"] == 1, (
        "the resume records the approved edit before the next turn starts"
    )

    unapproved = measured(
        [
            turn(),
            call("e1", "edit"),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"error": "skipped: x", "not_run": True}),
            turn(),
            call("b2", "bash", {"command": "cargo test"}),
        ]
    )
    assert unapproved["approval_resends"] == 0, (
        "an edit nobody approved is not a resend"
    )

    approved_without_skip = measured(
        [
            turn(),
            call("c1", "bash", {"command": "cargo test"}),
            decided("c1"),
            turn(),
            call("c2", "bash", {"command": "cargo test"}),
        ]
    )
    assert approved_without_skip["approval_resends"] == 0, (
        "an approved call without a withheld answer is not a resend"
    )

    different = measured(
        [
            turn(),
            call("e1", "edit"),
            asked("e1", "edit"),
            decided("e1", "Approved", "edit"),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"error": "skipped: x", "not_run": True}),
            turn(),
            call("b2", "bash", {"command": "cargo build"}),
        ]
    )
    assert different["approval_resends"] == 0

    denied = measured(
        [
            turn(),
            call("e1", "edit"),
            asked("e1", "edit"),
            decided("e1", "Denied", "edit"),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"error": "skipped: x", "not_run": True}),
            turn(),
            call("b2", "bash", {"command": "cargo test"}),
        ]
    )
    assert denied["approval_resends"] == 0, "a denial is not an approval"


def test_resend_anchors_on_the_edit_it_waited_for() -> None:
    failed_anchor = measured(
        [
            turn(),
            call("x9", "bash", {"command": "cargo build"}),
            decided("x9", "Approved", "bash"),
            call("e1", "edit"),
            answer("e1", {"error": "disk full"}),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"error": "skipped: x", "not_run": True}),
            turn(),
            call("b2", "bash", {"command": "cargo test"}),
        ]
    )
    assert failed_anchor["approval_resends"] == 0, (
        "an approval for another call does not release this check"
    )

    failed_but_approved = measured(
        [
            turn(),
            call("e1", "edit"),
            asked("e1", "edit"),
            decided("e1", "Approved", "edit"),
            answer("e1", {"error": "disk full"}),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"error": "skipped: x", "not_run": True}),
            turn(),
            call("b2", "bash", {"command": "cargo test"}),
        ]
    )
    assert failed_but_approved["approval_resends"] == 0, (
        "a failed edit is not a pending approval"
    )

    later_pending = measured(
        [
            turn(),
            call("e1", "edit"),
            answer("e1", {"error": "disk full"}),
            call("e2", "edit"),
            decided("e2", "Approved", "edit"),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"error": "skipped: x", "not_run": True}),
            turn(),
            call("b2", "bash", {"command": "cargo test"}),
        ]
    )
    assert later_pending["approval_resends"] == 0, (
        "the anchor is the first edit that did not complete, not a later one"
    )

    unrelated_approval = measured(
        [
            turn(),
            call("x9", "bash", {"command": "cargo build"}),
            decided("x9", "Approved"),
            call("e1", "edit"),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"error": "skipped: x", "not_run": True}),
            turn(),
            call("b2", "bash", {"command": "cargo test"}),
        ]
    )
    assert unrelated_approval["approval_resends"] == 0, (
        "an approval for another call is not this edit's answer"
    )

    ran_check = measured(
        [
            turn(),
            call("e1", "edit"),
            decided("e1", "Approved", "edit"),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"success": True, "stdout": "all good"}),
            turn(),
            call("b2", "bash", {"command": "cargo test"}),
        ]
    )
    assert ran_check["approval_resends"] == 0, (
        "a check that ran is not a withheld one"
    )

    completed_edit = measured(
        [
            turn(),
            call("e1", "edit"),
            decided("e1", "Approved", "edit"),
            answer("e1", {"success": True}),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"error": "skipped: x", "not_run": True}),
            turn(),
            call("b2", "bash", {"command": "cargo test"}),
        ]
    )
    assert completed_edit["approval_resends"] == 0, (
        "an edit that already answered does not anchor a later check"
    )

    duplicate_checks = measured(
        [
            turn(),
            call("e1", "edit"),
            decided("e1", "Approved", "edit"),
            call("b1", "bash", {"command": "cargo test"}),
            answer("b1", {"error": "skipped: x", "not_run": True}),
            call("b2", "bash", {"command": "cargo test"}),
            answer("b2", {"error": "skipped: x", "not_run": True}),
            answer("e1", {"success": True}),
            turn(),
            call("b3", "bash", {"command": "cargo test"}),
        ]
    )
    assert duplicate_checks["approval_resends"] == 1, (
        "one repeat answers both withheld checks"
    )


def test_shares_absent_without_edit_turns() -> None:
    counts = metrics.add_shares(measured([turn(), call("b1", "bash")]))
    assert counts["co_dispatch_share"] is None
    assert counts["split_share"] is None


def test_unusable_lines_are_counted_not_dropped() -> None:
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "log.jsonl"
        path.write_text(
            "not json\n"
            + json.dumps({"no_event": True}) + "\n"
            + json.dumps({"event": {"type": "TurnStarted", "turn": 1}}) + "\n",
            encoding="utf-8",
        )
        with contextlib.redirect_stderr(io.StringIO()):
            events, malformed = metrics.log_events(path)
    assert malformed == 2, malformed
    assert len(events) == 1, len(events)
    log = metrics.group_turns(events, malformed)
    assert metrics.measure(log, FACTS)["malformed_lines"] == 2


def test_unmatched_results_are_counted() -> None:
    log = metrics.group_turns([turn(), answer("ghost", {"ok": True})])
    assert log.unmatched == 1, log.unmatched


def test_main_continues_past_a_missing_log() -> None:
    with tempfile.TemporaryDirectory() as directory:
        session = Path(directory) / "session"
        session.mkdir()
        (session / "log.jsonl").write_text(
            json.dumps({"event": {"type": "TurnStarted", "turn": 1}}) + "\n",
            encoding="utf-8",
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(
            io.StringIO()
        ):
            code = metrics.main([str(Path(directory) / "missing"), str(session)])
    assert code == 2, code
    assert "Turn dispatch" in output.getvalue()


def main() -> int:
    checks = [
        test_rule_literals_follow_the_code,
        test_missing_predicate_names_it,
        test_reworded_skip_marker_is_rejected,
        test_turn_boundaries_group_calls,
        test_legacy_log_groups_on_user_input,
        test_legacy_prefix_keeps_its_turns,
        test_each_user_input_opens_a_turn,
        test_empty_model_turn_stays_a_turn,
        test_leading_input_shares_the_first_turn,
        test_tool_written_input_does_not_split_a_turn,
        test_answer_before_its_call_is_attached,
        test_co_dispatch_needs_the_edit_first,
        test_split_counts_the_following_turn,
        test_failed_anchor_counts_checks_in_both_turns,
        test_missed_and_pending_are_separate,
        test_failure_reading_matches_the_runtime,
        test_successful_edit_does_not_anchor_or_release,
        test_withheld_checks_use_the_skip_prefix,
        test_approval_resend_needs_a_withheld_check,
        test_resend_anchors_on_the_edit_it_waited_for,
        test_shares_absent_without_edit_turns,
        test_unusable_lines_are_counted_not_dropped,
        test_unmatched_results_are_counted,
        test_main_continues_past_a_missing_log,
    ]
    failures = []
    for check in checks:
        try:
            check()
        except AssertionError as error:
            failures.append(f"{check.__name__}: {error}")
    if failures:
        for failure in failures:
            print(f"FAIL: {failure}", file=sys.stderr)
        print(f"\n[turn-dispatch-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[turn-dispatch-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())