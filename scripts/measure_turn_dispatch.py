#!/usr/bin/env python3
"""Measure how file edits and the shell checks after them are dispatched.

A session log records model calls and the answers the host gave them. This
tool counts, for each log, how often an edit and its check share one turn,
how often the check falls into the next turn, how many checks follow an
edit that failed or did not run, how many the edit-check rule withheld,
and how many withheld checks the model sent again after an approval.

The tool is read-only: it opens the log, never writes to it, and never
touches the session directory. Family membership, the failure words, and
the skip prefix are read from the same Rust source the runtime rule
matches. A rewrite that moves membership out of those literals, or adds a
condition beside them, is invisible to this text scan, so the test states
the membership it expects and fails when that source changes.

Definitions used by every count:

- A turn starts at a TurnStarted record. A log written before that record
  existed groups on UserInput instead.
- An edit call is one whose tool name is in the file-writing family; a
  shell check is one whose tool name is in the shell family. The rule
  withholds every shell call after an edit that did not complete, so the
  measurement counts every shell call as a check.
- An edit failed when its answer reports a real execution failure, read
  the way the runtime reads one; an edit is missed when it failed or
  carries the not-run marker; an edit is pending when no answer was
  recorded. These are the outcomes the runtime rule separates.
- A withheld check is an answer whose error carries the rule's skip prefix.
- An approval resend is a withheld check whose anchor edit had no answer
  yet when the check was withheld and was approved, sent again later with
  the same tool and input. The anchor is the first edit of the turn that
  did not complete, which is the one the runtime rule holds the check
  behind, and its answer may arrive after the check because the resume
  that records it runs before the next turn starts. That is the check the
  prompt tells the model to repeat once the edit is approved.
- A turn co-dispatches when an edit call appears before a shell call in it.
  A turn splits when it holds an edit and no shell call and the next turn
  holds a shell call.
"""

import argparse
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

REPO = Path(__file__).resolve().parent.parent
TOOL_SOURCE = REPO / "crates/houyicoder-api/src/tool.rs"
FALLBACK_SOURCE = REPO / "crates/houyicoder-core/src/agent/turn/fallback.rs"
TOOL_STATS_SOURCE = REPO / "crates/houyicoder-core/src/observability/tool_stats.rs"

SKIP_PREFIX = "skipped:"
EDIT_FAMILY = "writes_files"
SHELL_FAMILY = "is_shell_tool"
FAILURE_READER = "tool_failure_reason"

COUNT_KEYS = (
    "turns",
    "tool_calls",
    "edit_calls",
    "shell_calls",
    "edit_turns",
    "co_dispatch_turns",
    "split_turns",
    "failed_edit_calls",
    "missed_edit_calls",
    "not_run_edit_calls",
    "pending_edit_calls",
    "checks_after_failed_edit",
    "checks_after_missed_edit",
    "checks_after_pending_edit",
    "shells_next_turn_after_failed_edit",
    "shells_next_turn_after_missed_edit",
    "withheld_checks",
    "approval_asks",
    "approval_decisions",
    "approval_resends",
    "malformed_lines",
    "unmatched_results",
)


class FamilySourceError(Exception):
    """The rule's own predicates could not be read from their source."""


@dataclass(frozen=True)
class RuleFacts:
    """The families, failure words, and skip prefix the runtime rule defines."""

    writes: frozenset[str]
    shells: frozenset[str]
    failure_words: frozenset[str]
    skip_prefix: str


@dataclass
class Call:
    """One tool call, with the answer the host recorded for it.

    The two positions order the call and its answer against the rest of the
    log, so a state read at one point in the session is not confused with
    the state the log ends in.
    """

    call_id: str
    tool: str
    input: Any
    turn: int
    position: int = -1
    result: Any = None
    result_position: int | None = None


@dataclass
class Turn:
    """One turn's calls in the order they were emitted."""

    index: int
    calls: list[Call] = field(default_factory=list)


@dataclass
class SessionLog:
    """One log's turns, calls, permission records, and unreadable data."""

    turns: list[Turn]
    calls: list[Call]
    approved: set[str]
    asks: int
    decisions: int
    malformed: int
    unmatched: int


def function_body(source: str, name: str) -> str:
    """Return the braced body of the function named name."""
    start = source.find("fn " + name + "(")
    if start < 0:
        raise FamilySourceError("no function named " + name + " in the predicates")
    open_brace = source.find("{", start)
    if open_brace < 0:
        raise FamilySourceError("no body for " + name)
    depth = 0
    for index in range(open_brace, len(source)):
        char = source[index]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return source[open_brace + 1 : index]
    raise FamilySourceError("unclosed body for " + name)


def literals_in(source: str, name: str) -> frozenset[str]:
    """Return the string literals a family predicate matches."""
    values = re.findall(r'"([^"]*)"', function_body(source, name))
    if not values:
        raise FamilySourceError("no literals in " + name)
    upper = [value for value in values if value != value.lower()]
    if upper:
        raise FamilySourceError("uppercase literal in " + name + ": " + ", ".join(upper))
    return frozenset(values)


def skip_prefix(source: str) -> str:
    """Return the skip prefix after checking the rule's marker uses it."""
    match = re.search(r'SKIPPED_AFTER_EDIT\s*:\s*&str\s*=\s*"([^"]*)"', source)
    if not match:
        raise FamilySourceError("no SKIPPED_AFTER_EDIT constant in the fallback module")
    literal = match.group(1)
    if not literal.startswith(SKIP_PREFIX):
        raise FamilySourceError(
            "the skip marker no longer starts with " + SKIP_PREFIX + ": " + literal
        )
    return SKIP_PREFIX


def failure_words(source: str) -> frozenset[str]:
    """Return the words the runtime reads as a failure clue in stdout."""
    values = re.findall(
        r'contains\("([^"]+)"\)', function_body(source, FAILURE_READER)
    )
    if not values:
        raise FamilySourceError("no failure words in " + FAILURE_READER)
    upper = [value for value in values if value != value.lower()]
    if upper:
        raise FamilySourceError(
            "uppercase failure word in " + FAILURE_READER + ": " + ", ".join(upper)
        )
    return frozenset(values)


def load_rule_facts(
    tool_source: Path = TOOL_SOURCE,
    fallback_source: Path = FALLBACK_SOURCE,
    tool_stats_source: Path = TOOL_STATS_SOURCE,
) -> RuleFacts:
    """Read the families, failure words, and skip prefix from the rule."""
    tool_text = tool_source.read_text(encoding="utf-8")
    fallback_text = fallback_source.read_text(encoding="utf-8")
    tool_stats_text = tool_stats_source.read_text(encoding="utf-8")
    return RuleFacts(
        writes=literals_in(tool_text, EDIT_FAMILY),
        shells=literals_in(tool_text, SHELL_FAMILY),
        failure_words=failure_words(tool_stats_text),
        skip_prefix=skip_prefix(fallback_text),
    )


def resolve_log(target: str) -> Path:
    """Return the log file for a session directory or a log path."""
    path = Path(target)
    if path.is_dir():
        path = path / "log.jsonl"
    return path


def log_events(path: Path) -> tuple[list[dict], int]:
    """Return the event objects in a log, plus the count of unusable lines."""
    events: list[dict] = []
    malformed = 0
    with path.open(encoding="utf-8") as handle:
        for line_no, line in enumerate(handle, 1):
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                malformed += 1
                print(f"{path}:{line_no}: not JSON, skipped", file=sys.stderr)
                continue
            event = entry.get("event") if isinstance(entry, dict) else None
            if not isinstance(event, dict):
                malformed += 1
                print(f"{path}:{line_no}: no event record, skipped", file=sys.stderr)
                continue
            events.append(event)
    return events, malformed


def group_turns(events: list[dict], malformed: int = 0) -> SessionLog:
    """Group events into turns and attach each answer to its call.

    A turn starts at a TurnStarted record. A log written before that record
    existed groups on UserInput instead; the input the host writes before
    the first model call and the boundary that follows it share one turn.
    """
    turns: list[Turn] = []
    calls: list[Call] = []
    by_id: dict[str, Call] = {}
    early: dict[str, tuple[int, Any]] = {}
    approved: set[str] = set()
    asks = 0
    decisions = 0
    unmatched = 0
    current: Turn | None = None
    current_from_input = False
    boundary_seen = False

    def start_turn(from_input: bool) -> Turn:
        nonlocal current, current_from_input
        if (
            not from_input
            and current_from_input
            and current is not None
            and not current.calls
        ):
            current_from_input = False
            return current
        current = Turn(index=len(turns))
        current_from_input = from_input
        turns.append(current)
        return current

    for position, event in enumerate(events):
        event_type = event.get("type")
        if event_type == "TurnStarted":
            start_turn(from_input=False)
            boundary_seen = True
        elif event_type == "UserInput" and not boundary_seen:
            start_turn(from_input=True)
        elif event_type == "ToolCall":
            if current is None:
                start_turn(from_input=False)
            call_id = str(event.get("call_id", ""))
            call = Call(
                call_id=call_id,
                tool=str(event.get("tool", "")),
                input=event.get("input"),
                turn=current.index,
                position=position,
            )
            if call_id in early:
                call.result_position, call.result = early.pop(call_id)
            current.calls.append(call)
            calls.append(call)
            by_id[call_id] = call
        elif event_type == "ToolResult":
            call_id = str(event.get("call_id", ""))
            call = by_id.get(call_id)
            if call is None:
                unmatched += 1
                early[call_id] = (position, event.get("output"))
            else:
                call.result = event.get("output")
                call.result_position = position
        elif event_type == "PermissionAsked":
            asks += 1
        elif event_type == "PermissionDecision":
            decisions += 1
            if str(event.get("verdict", "")).lower() == "approved":
                approved.add(str(event.get("call_id", "")))

    return SessionLog(turns, calls, approved, asks, decisions, malformed, unmatched)


def family(call: Call, facts: RuleFacts) -> str:
    """Classify a call as an edit, a shell check, or neither."""
    name = call.tool.lower()
    if name in facts.writes:
        return "edit"
    if name in facts.shells:
        return "shell"
    return "other"


def tool_result_not_run(result: Any) -> bool:
    """Whether the answer says the call was recorded without running."""
    return isinstance(result, dict) and result.get("not_run") is True


def tool_failure_reason(result: Any, words: frozenset[str]) -> str | None:
    """Return the failure text the runtime reads from an answer, if any."""
    if not isinstance(result, dict):
        return None
    error = result.get("error")
    if isinstance(error, str):
        return error
    if result.get("success") is not False:
        return None
    stderr = result.get("stderr") if isinstance(result.get("stderr"), str) else ""
    if stderr:
        return stderr.split("\n")[0]
    stdout = result.get("stdout") if isinstance(result.get("stdout"), str) else ""
    for line in reversed(stdout.split("\n")):
        lowered = line.lower()
        if any(word in lowered for word in words):
            return line
    return None


def ran_and_failed(result: Any, words: frozenset[str]) -> bool:
    """Whether the answer reports a real execution failure, not a withheld call."""
    return not tool_result_not_run(result) and tool_failure_reason(result, words) is not None


def missed(result: Any, words: frozenset[str]) -> bool:
    """Whether the answer says the edit is over and wrote nothing."""
    return tool_result_not_run(result) or tool_failure_reason(result, words) is not None


def pending(call: Call) -> bool:
    """Whether the log holds no answer for the call at all."""
    return call.result_position is None


def pending_at(call: Call, position: int) -> bool:
    """Whether the call had no answer yet when position was reached."""
    return call.result_position is None or call.result_position > position


def withheld(result: Any, prefix: str) -> bool:
    """Whether an answer carries the edit-check rule's skip marker."""
    if not isinstance(result, dict):
        return False
    error = result.get("error")
    return isinstance(error, str) and error.startswith(prefix)


def turn_shapes(turns: list[Turn], facts: RuleFacts) -> tuple[int, int, int]:
    """Return edit turns, co-dispatched turns, and split turns."""
    edit_turns = 0
    co_dispatch = 0
    for turn in turns:
        roles = [family(call, facts) for call in turn.calls]
        if "edit" not in roles:
            continue
        edit_turns += 1
        first_edit = roles.index("edit")
        if "shell" in roles[first_edit + 1 :]:
            co_dispatch += 1
    split = 0
    for turn, following in zip(turns, turns[1:]):
        roles = [family(call, facts) for call in turn.calls]
        if "edit" in roles and "shell" not in roles:
            if any(family(call, facts) == "shell" for call in following.calls):
                split += 1
    return edit_turns, co_dispatch, split


def after_edits(
    turns: list[Turn], facts: RuleFacts, anchors: Callable[[Call], bool]
) -> tuple[int, int]:
    """Return same-turn and next-turn shell counts after an anchor edit.

    The first edit of a turn whose call matches anchors holds every shell
    call after it in that turn, and every shell call in the following turn.
    A later edit that succeeds does not release them, which is how the
    runtime rule reads the same turn.
    """
    same_turn = 0
    anchor_turns = set()
    for turn in turns:
        anchored = False
        for call in turn.calls:
            role = family(call, facts)
            if role == "edit" and not anchored and anchors(call):
                anchored = True
                anchor_turns.add(turn.index)
            elif role == "shell" and anchored:
                same_turn += 1
    next_turn = 0
    for turn in turns:
        if turn.index - 1 in anchor_turns:
            next_turn += sum(
                1 for call in turn.calls if family(call, facts) == "shell"
            )
    return same_turn, next_turn


def approval_resends(log: SessionLog, facts: RuleFacts) -> int:
    """Count withheld checks repeated later after their anchor edit was approved.

    The anchor is read as of the moment the check was withheld. An edit whose
    answer the resume recorded later in the same turn was still waiting for
    that answer when the check was held, so a completed log and a paused one
    read the same way; only the state at the withholding point decides.
    """
    resends = 0
    used: set[int] = set()
    for turn in log.turns:
        for call in turn.calls:
            if family(call, facts) != "shell":
                continue
            if not withheld(call.result, facts.skip_prefix):
                continue
            point = call.result_position
            if point is None:
                continue
            anchor = next(
                (
                    earlier
                    for earlier in turn.calls
                    if earlier.position < point
                    and family(earlier, facts) == "edit"
                    and (
                        pending_at(earlier, point)
                        or missed(earlier.result, facts.failure_words)
                    )
                ),
                None,
            )
            if anchor is None:
                continue
            if not (pending_at(anchor, point) and anchor.call_id in log.approved):
                continue
            later = next(
                (
                    index
                    for index, candidate in enumerate(log.calls)
                    if index not in used
                    and candidate.turn > turn.index
                    and candidate.tool == call.tool
                    and candidate.input == call.input
                ),
                None,
            )
            if later is None:
                continue
            used.add(later)
            resends += 1
    return resends


def measure(log: SessionLog, facts: RuleFacts) -> dict[str, int]:
    """Compute every count for one log."""
    words = facts.failure_words
    roles = [family(call, facts) for call in log.calls]
    edit_turns, co_dispatch, split = turn_shapes(log.turns, facts)
    failed_same, failed_next = after_edits(
        log.turns, facts, lambda call: ran_and_failed(call.result, words)
    )
    missed_same, missed_next = after_edits(
        log.turns, facts, lambda call: missed(call.result, words)
    )
    pending_same, _ = after_edits(log.turns, facts, pending)
    counts = {
        "turns": len(log.turns),
        "tool_calls": len(log.calls),
        "edit_calls": roles.count("edit"),
        "shell_calls": roles.count("shell"),
        "edit_turns": edit_turns,
        "co_dispatch_turns": co_dispatch,
        "split_turns": split,
        "failed_edit_calls": sum(
            1 for call, role in zip(log.calls, roles)
            if role == "edit" and ran_and_failed(call.result, words)
        ),
        "missed_edit_calls": sum(
            1 for call, role in zip(log.calls, roles)
            if role == "edit" and missed(call.result, words)
        ),
        "not_run_edit_calls": sum(
            1 for call, role in zip(log.calls, roles)
            if role == "edit" and tool_result_not_run(call.result)
        ),
        "pending_edit_calls": sum(
            1 for call, role in zip(log.calls, roles)
            if role == "edit" and pending(call)
        ),
        "checks_after_failed_edit": failed_same,
        "checks_after_missed_edit": missed_same,
        "checks_after_pending_edit": pending_same,
        "shells_next_turn_after_failed_edit": failed_next,
        "shells_next_turn_after_missed_edit": missed_next,
        "withheld_checks": sum(
            1 for call in log.calls if withheld(call.result, facts.skip_prefix)
        ),
        "approval_asks": log.asks,
        "approval_decisions": log.decisions,
        "approval_resends": approval_resends(log, facts),
        "malformed_lines": log.malformed,
        "unmatched_results": log.unmatched,
    }
    return counts


def add_shares(counts: dict[str, int]) -> dict[str, Any]:
    """Return the counts plus the two shares, absent when no edit turn exists."""
    entry: dict[str, Any] = dict(counts)
    edit_turns = counts["edit_turns"]
    if edit_turns:
        entry["co_dispatch_share"] = round(counts["co_dispatch_turns"] / edit_turns, 4)
        entry["split_share"] = round(counts["split_turns"] / edit_turns, 4)
    else:
        entry["co_dispatch_share"] = None
        entry["split_share"] = None
    return entry


def totals(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """Sum the counts across logs and recompute the shares from the sums."""
    summed = {key: sum(row[key] for row in rows) for key in COUNT_KEYS}
    return add_shares(summed)


def percent(share: Any) -> str:
    """Render a share as a percentage, or a dash when absent."""
    return "-" if share is None else f"{share * 100:.1f}%"


def render_markdown(rows: list[dict[str, Any]], all_rows: dict[str, Any]) -> str:
    """Render the counts as a Markdown table plus the definitions in force."""
    header = (
        "| log | turns | calls | edits | checks | co-dispatch | split | "
        "failed edits | missed edits | checks after failed/missed/pending | "
        "next-turn shells failed/missed | withheld | resends |"
    )
    rule = "|" + "---|" * 13
    lines = ["## Turn dispatch", "", header, rule]
    for row in rows + [dict(all_rows, log="total")]:
        lines.append(
            f"| {row['log']} | {row['turns']} | {row['tool_calls']} | "
            f"{row['edit_calls']} | {row['shell_calls']} | "
            f"{row['co_dispatch_turns']} | {row['split_turns']} | "
            f"{row['failed_edit_calls']} | {row['missed_edit_calls']} | "
            f"{row['checks_after_failed_edit']} / "
            f"{row['checks_after_missed_edit']} / "
            f"{row['checks_after_pending_edit']} | "
            f"{row['shells_next_turn_after_failed_edit']} / "
            f"{row['shells_next_turn_after_missed_edit']} | "
            f"{row['withheld_checks']} | {row['approval_resends']} |"
        )
    lines.extend(
        [
            "",
            "Co-dispatch share: "
            + percent(all_rows["co_dispatch_share"])
            + " of turns holding an edit; split share: "
            + percent(all_rows["split_share"])
            + " of turns holding an edit.",
            "Edit and shell membership is read from the runtime rule's own "
            "predicates; every shell call counts as a check.",
            "The rule withholds checks within one response, so the same-turn "
            "columns mirror it. The next-turn columns describe shell calls "
            "that followed, not calls it withheld.",
        ]
    )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """Parse arguments, measure each log, and print JSON or Markdown."""
    parser = argparse.ArgumentParser(
        description="Measure how edits and their shell checks are dispatched across turns."
    )
    parser.add_argument("logs", nargs="+", help="session directory or log.jsonl path")
    parser.add_argument("--json", action="store_true", help="emit JSON instead of Markdown")
    args = parser.parse_args(argv)

    try:
        facts = load_rule_facts()
    except (OSError, FamilySourceError) as error:
        print(f"cannot read the rule's own definitions: {error}", file=sys.stderr)
        return 2

    rows: list[dict[str, Any]] = []
    errors: list[str] = []
    for target in args.logs:
        path = resolve_log(target)
        try:
            events, malformed = log_events(path)
        except OSError as error:
            message = f"{path}: {error}"
            errors.append(message)
            print(f"cannot read {message}", file=sys.stderr)
            continue
        log = group_turns(events, malformed)
        rows.append(dict(add_shares(measure(log, facts)), log=str(path)))

    if not rows:
        print("no readable logs", file=sys.stderr)
        return 2
    if args.json:
        print(
            json.dumps(
                {"sessions": rows, "totals": totals(rows), "errors": errors},
                indent=2,
            )
        )
    else:
        print(render_markdown(rows, totals(rows)))
    return 2 if errors else 0


if __name__ == "__main__":
    sys.exit(main())