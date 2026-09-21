#!/usr/bin/env python3
"""Regression tests for the commit gate (hook_commit_gate.py).

The gate is the machine backstop for the commit discipline -- deep-review
first, message shown to the user, then a marker -- so the gate itself has
to be guarded, the same reason test_stderr_gate.py and
test_harness_routing.py exist.

Most cases come in twos: a commit form that must be gated sits beside a
lookalike that must not be, so an implementation that reports every
command as a commit fails just as loudly as one that reports none. Where
no lookalike exists, gating is the deliberate direction of the error. What
carries the weight is where the same characters mean different things: a
flag quoted in a message against the subcommand's own help request, a
shell given a command line against a shell given a file to run, and a
payload the gate cannot read against a tool call that is not a shell
call.

Run: python3 scripts/test_hook_commit_gate.py  (wired into make check as
commit-gate-tests). Exit 0 = pass or no hook to test, 1 = fail.
"""
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


def hook_path():
    """The hook under test, resolved the way the guard that runs it resolves.

    This tree's copy first, then the main checkout's through the git common
    dir: an ignored internal hook is absent from a worktree, and the guard
    that runs it as a PreToolUse hook falls back to the main checkout, so
    the test reads the file that would gate the commit. A clone carrying
    neither copy has no gate to test.
    """
    here = Path(__file__).resolve().parent
    local = here / "hook_commit_gate.py"
    if local.exists():
        return local
    try:
        common = subprocess.run(
            ["git", "-C", str(here), "rev-parse", "--git-common-dir"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None
    if not common:
        return None
    root = Path(common)
    if not root.is_absolute():
        root = here / root
    return root.resolve().parent / "scripts" / "hook_commit_gate.py"


HOOK = hook_path()
if HOOK is None or not HOOK.exists():
    print("skip commit-gate-tests: no copy of the hook in this checkout or the main one")
    sys.exit(0)

sys.path.insert(0, str(HOOK.parent))
from hook_commit_gate import runs_commit  # noqa: E402

GATED = [
    "git commit -m x",
    "git commit --amend --no-edit",
    "git -C /Users/von/workspace/hicoder/wiki commit -F msg.txt",
    "git -c user.name=vongosling commit -m x",
    "cd /Users/von/workspace/hicoder/wiki && git commit -m x",
    "/usr/bin/git commit -m x",
    "git --no-pager commit -m x",
    # A commit wrapped in a shell or in eval is a commit. The nested line is
    # a separate command line to read, not one word to compare, and a shell
    # takes options of its own before the flag that carries it.
    "bash -c 'git commit -m x'",
    "sh -c 'git -C /Users/von/workspace/hicoder/wiki commit -F msg'",
    "bash -lc 'git commit -m x'",
    "bash -euo pipefail -c 'git commit -m x'",
    "bash --norc -c 'git commit -m x'",
    "eval 'git commit -m x'",
    # A backslash at the end of a line continues it. The shell runs the
    # joined words, so a gate reading the line as written reads two lines
    # where the shell runs one.
    "git -C /Users/von/workspace/hicoder/wiki \\\n  commit -F msg.txt",
    "git \\\n  -C /Users/von/workspace/hicoder/wiki commit -m x",
    "bash -c \\\n  'git commit -m x'",
    # The message mentions a flag; the flag is not the command's own help
    # request, so the commit still runs and must still be gated. The same
    # goes for a flag read as an option value or as a pathspec.
    "git commit -m 'clarify the --help output'",
    "git commit -m --help",
    "git commit -- --help",
    # A help flag past the subcommand's first word is read as a commit: the
    # word sits where an option value or a pathspec sits, and gating a help
    # request costs a marker while exempting a commit costs the review.
    "git commit -m x --help",
    # The words appear in an echo, which runs no commit at all. Gating it is
    # the deliberate direction of the error: a false positive asks for a
    # marker, a false negative lets an unreviewed commit through.
    "echo git commit",
    # A redirection with no terminator line is not a body this can drop, so
    # the words stay for the scan: the same direction of error as the echo
    # above, rather than dropping lines a shell would run.
    "cat <<EOF\ngit commit -m x\n",
]

NOT_GATED = [
    "git status --short",
    "git -C /Users/von/workspace/hicoder/wiki status --short",
    "git -C /Users/von/workspace/hicoder/wiki add -A",
    "git log --oneline -20",
    "git log --grep=commit",
    "git config --get user.name",
    "git clean -n",
    "git checkout dev",
    # A help request runs no commit, so it needs no marker. Read against the
    # quoting, value, and pathspec cases above, which must not read as one.
    "git commit --help",
    "git -h commit",
    "git --help commit",
    # git takes no abbreviated subcommand, so these run nothing. The two
    # below are real subcommands that begin with the commit letters, which
    # a prefix comparison would read as a commit.
    "git commi -m x",
    "git -C /Users/von/workspace/hicoder/wiki commi -m x",
    "git commit-tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904 -m x",
    "git commit-graph write",
    "bash -c 'git status --short'",
    "bash script.sh -c foo",
    "python3 -c 'print(\"git commit\")'",
    # A heredoc body is data being written to a file, not words a command
    # runs, so the commit line inside one is text and nothing else. The
    # second case puts a commit on the line after the terminator, which the
    # shell does run.
    "cat > /tmp/notes.md <<'EOF'\ngit -C /x commit -m y\nEOF\n",
    "cat > /tmp/notes.md <<EOF\ngit commit -m y\nEOF\ngit status --short\n",
    "make check",
    "touch .claude/.commit_ready",
]


def run_hook(command, cwd, project=None, tool="Bash", raw=None):
    """Run the hook as the harness does: JSON on stdin, cwd as given."""
    payload = raw if raw is not None else json.dumps(
        {"tool_name": tool, "tool_input": {"command": command}}
    )
    env = dict(os.environ)
    if project is None:
        env.pop("CLAUDE_PROJECT_DIR", None)
    else:
        env["CLAUDE_PROJECT_DIR"] = str(project)
    proc = subprocess.run(
        [sys.executable, HOOK],
        input=payload,
        capture_output=True,
        text=True,
        cwd=str(cwd),
        env=env,
    )
    return proc.returncode, proc.stdout, proc.stderr


def set_marker(tmp, name="cwd"):
    """A tree with a marker set; returns the tree root."""
    root = Path(tempfile.mkdtemp()) / name
    (root / ".claude").mkdir(parents=True)
    (root / ".claude" / ".commit_ready").touch()
    return root


def test_detection():
    for command in GATED:
        assert runs_commit(command), f"not gated: {command}"
    for command in NOT_GATED:
        assert not runs_commit(command), f"wrongly gated: {command}"
    print("ok  detection: commit subcommand through options, shells, and separators")


def test_commit_without_marker_is_blocked():
    root = Path(tempfile.mkdtemp())
    code, out, err = run_hook("git commit -m x", root)
    assert code == 2, f"exit {code} for a commit with no marker"
    # The harness renders the model-facing error from stderr, so the
    # guidance has to be there and not only on stdout.
    assert "BLOCKED" in err, "the block message is not on stderr"
    assert not out.strip(), "the block message went to the model-blind stream"
    assert not (root / ".claude" / ".commit_ready").exists(), "the block wrote a marker"
    print("ok  block: a commit with no marker exits 2 and leaves no marker")


def test_commit_with_marker_passes_once():
    root = set_marker(Path(tempfile.mkdtemp()))
    code, _, _ = run_hook("git -C /elsewhere commit -m x", root)
    assert code == 0, f"exit {code} for a commit with a marker set"
    assert not (root / ".claude" / ".commit_ready").exists(), "marker survived"
    code, _, _ = run_hook("git commit -m x", root)
    assert code == 2, "a second commit reused the consumed marker"
    print("ok  one-shot: the commit with a marker passes and the next one is blocked")


def test_marker_found_in_project_dir():
    home = Path(tempfile.mkdtemp())
    project = set_marker(Path(tempfile.mkdtemp()), name="shared")
    code, _, _ = run_hook("git commit -m x", home, project=project)
    assert code == 0, f"exit {code} for a marker in the project dir"
    assert not (project / ".claude" / ".commit_ready").exists(), "marker survived"
    print("ok  per-tree: a marker in the project directory is honored")


def test_other_shell_call_clears_pending_marker():
    root = set_marker(Path(tempfile.mkdtemp()))
    code, _, _ = run_hook("ls -la", root)
    assert code == 0, f"exit {code} for a plain shell call"
    assert not (root / ".claude" / ".commit_ready").exists(), "pending marker kept"
    code, _, _ = run_hook("git commit -m x", root)
    assert code == 2, "a commit ran on a marker an earlier call should have cleared"
    print("ok  adjacency: another shell call clears a marker that outlived its commit")


def test_lookalike_shell_call_is_not_gated():
    root = set_marker(Path(tempfile.mkdtemp()))
    code, _, _ = run_hook("git -C /elsewhere status --short", root)
    assert code == 0, f"exit {code} for a non-commit git call"
    print("ok  control: a non-commit git call is not blocked")


def test_non_shell_call_keeps_marker():
    root = set_marker(Path(tempfile.mkdtemp()))
    code, _, _ = run_hook("git commit -m x", root, tool="Read")
    assert code == 0, f"exit {code} for a non-shell tool"
    assert (root / ".claude" / ".commit_ready").exists(), "a non-shell call cleared it"
    print("ok  scope: only shell calls touch the marker")


def test_commit_help_needs_no_marker():
    root = Path(tempfile.mkdtemp())
    code, _, _ = run_hook("git commit --help", root)
    assert code == 0, f"exit {code} for a help request"
    print("ok  help: a help request runs no commit and is not blocked")


def test_malformed_payload_clears():
    for raw in (
        "[]",
        '[{"tool_name": "Bash"}]',
        '{"tool_name": "Bash", "tool_input": "git commit -m x"}',
        '{"tool_name": "Bash", "tool_input": {"command": null}}',
        # A dict that names no tool, or names one in a shape the gate cannot
        # read, is not a call it can tell apart from a shell call.
        "{}",
        '{"tool_name": null}',
        '{"tool_name": ["Bash"]}',
        '{"tool_input": {"command": "git commit -m x"}}',
    ):
        root = set_marker(Path(tempfile.mkdtemp()))
        code, _, _ = run_hook("", root, raw=raw)
        assert code == 0, f"exit {code} for payload {raw}"
        assert not (root / ".claude" / ".commit_ready").exists(), (
            f"a payload the gate cannot read kept the marker: {raw}"
        )
    print("ok  input: a shape the gate cannot read clears rather than crashing")


def test_unparseable_input_passes():
    root = set_marker(Path(tempfile.mkdtemp()))
    code, _, _ = run_hook("", root, raw="not json")
    assert code == 0, f"exit {code} for unreadable hook input"
    assert not (root / ".claude" / ".commit_ready").exists(), (
        "a payload the gate cannot read kept the marker"
    )
    unbalanced = json.dumps({"tool_name": "Bash", "tool_input": {"command": "git commit -m 'x"}})
    code, _, _ = run_hook("", root, raw=unbalanced)
    assert code == 2, "a commit with unbalanced quoting slipped through"
    print("ok  input: unreadable JSON passes and clears, unbalanced quoting still gates")


def test_unremovable_marker_blocks_commit():
    # A marker path that cannot be removed, here a directory, is a marker
    # the gate could not consume. Crashing would leave the harness reading
    # no objection while the marker stayed for the next commit, so the call
    # answers instead: a shell call is not failed by it, a commit does not
    # count it, and the block names the path, because the same message as a
    # missing marker would leave the model retrying a state that never
    # clears with nothing to act on.
    root = Path(tempfile.mkdtemp())
    stuck = root / ".claude" / ".commit_ready"
    stuck.mkdir(parents=True)
    code, _, err = run_hook("ls -la", root)
    assert code == 0, f"exit {code} for a shell call over an unremovable marker"
    assert "Traceback" not in err, "the removal crashed instead of answering"
    code, _, err = run_hook("git commit -m x", root)
    assert code == 2, "a commit counted a marker the gate could not drop"
    assert str(stuck) in err, f"the block did not name the marker it kept: {err}"
    # A marker further along the list is still attempted: stopping at the
    # first failure would leave it on disk for the tree it belongs to.
    project = set_marker(Path(tempfile.mkdtemp()), name="shared")
    code, _, _ = run_hook("git commit -m x", root, project=project)
    assert code == 2, "the commit ran on a marker the gate could not drop"
    assert not (project / ".claude" / ".commit_ready").exists(), (
        "a marker that could not be dropped left the next one in place"
    )
    print("ok  state: a marker that cannot be dropped is reported, not counted")


def main():
    test_detection()
    test_commit_without_marker_is_blocked()
    test_commit_with_marker_passes_once()
    test_marker_found_in_project_dir()
    test_other_shell_call_clears_pending_marker()
    test_lookalike_shell_call_is_not_gated()
    test_non_shell_call_keeps_marker()
    test_commit_help_needs_no_marker()
    test_malformed_payload_clears()
    test_unparseable_input_passes()
    test_unremovable_marker_blocks_commit()
    print("PASS test_hook_commit_gate")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except AssertionError as failure:
        print(f"FAIL {failure}")
        sys.exit(1)
