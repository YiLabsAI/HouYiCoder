#!/usr/bin/env python3
"""Regression tests for the pre-push chain and the installer that lays it down.

Two scripts are under test. scripts/hooks/pre-push is the chain git runs: it
exists because two guards both want the pre-push slot, a local one a developer
installed first and the repository's own range re-scan of the commits about to
be published. Installing one used to displace the other, which is how the range
re-scan went silently missing on a machine whose hook slot was already taken.
The chain has one trap worth pinning: git hands the ref list to a pre-push hook
on stdin, which is readable once, so the chain captures it and replays it to
every guard. A guard that reads an empty list checks nothing and reports
success, so a chain that forgets to replay is a false green, not a loud failure.

scripts/hooks/install.sh is the installer. It is the half that can destroy
something: the hook slot may hold a guard that blocks publishing pre-release
history, and an installer that misclassifies it either runs the repository gate
twice or overwrites the guard while printing a success message. Recognition is
therefore exact. A chain is known by the marker on its second line, not by that
string appearing anywhere in the file, because a guard that checks whether the
chain is installed carries the same string in its body. The single-guard
wrapper — all it does is delegate the whole push to the repository gate — is
known by its whole content, so a developer who added checks above the
delegation still has a guard, and one who only added a blank line at the end
does not have the repository gate run twice.
Either way nothing is discarded: a guard moves to pre-push.local and the
installer's own superseded output moves to pre-push.replaced.

The installer cases share one shape — what holds the slot decides where every
file lands — so they run from one table rather than one function per input.
Each row states its occupant, the exit it expects, and where the occupant and
the chain must end up; a row whose behaviour needs more than that shape holds
(the failure rows, whose point is the lie in the success message) keeps its own
function. The last rows hold the installer to the other half of the same rule:
a step that cannot complete must fail the install instead of reporting one,
because a slot that looks gated and is not runs no gate at all while every push
goes through.

Both are hermetic by construction rather than by luck. Each chain case
initialises a throwaway tree as a git repository, because the chain resolves
the repository gate through git rev-parse from its working directory: without
that, a temporary directory landing inside any repository would point the case
at that repository's real gate. Each installer case passes a throwaway hooks
directory, leaving the shared one git reports for this repository untouched.

Run: python3 scripts/test_pre_push_chain.py  (make check runs it).
Exit 0 = pass, 1 = fail.
"""
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
REPO = SCRIPTS.parent
HOOK = SCRIPTS / "hooks" / "pre-push"
INSTALLER = SCRIPTS / "hooks" / "install.sh"
MARKER = "houyicoder installed pre-push"
REFS = "refs/heads/dev aaa bbb\nrefs/heads/main ccc ddd\n"
SINGLE_GUARD_WRAPPER = (
    "#!/usr/bin/env bash\n"
    "set -euo pipefail\n"
    "repo=$(git rev-parse --show-toplevel 2>/dev/null || pwd)\n"
    '[ -f "$repo/scripts/pre_push_hook.sh" ] || exit 0\n'
    'exec bash "$repo/scripts/pre_push_hook.sh"\n'
)

# A wrapper a developer personalised by adding checks above the delegation it
# ends with. It is no longer the installer's own output, so it must stay a
# guard rather than be replaced as though it were.
CUSTOMISED_WRAPPER = (
    "#!/usr/bin/env bash\n"
    "set -euo pipefail\n"
    "echo 'no publishing until the changelog is written'\n"
    "[ -s CHANGELOG.md ] || exit 1\n"
    'repo=$(git rev-parse --show-toplevel 2>/dev/null || pwd)\n'
    '[ -f "$repo/scripts/pre_push_hook.sh" ] || exit 0\n'
    'exec bash "$repo/scripts/pre_push_hook.sh"\n'
)

# A publish blocker that refuses to push while the chain is missing, detecting
# it by looking for the marker — so the marker sits in the guard's body.
MARKER_CHECKING_GUARD = (
    "#!/bin/sh\n"
    "# refuses to publish while the chain is missing\n"
    'grep -q "$MARKER" "$HOOK_DIR/pre-push" || exit 1\n'
    f"# {MARKER}\n"
    "exit 0\n"
)


class Chain:
    """A throwaway repository holding the installed hook and the guards it chains."""

    def __init__(self, tmp: Path):
        self.root = tmp
        (tmp / "hooks").mkdir(parents=True)
        (tmp / "scripts").mkdir(parents=True)
        subprocess.run(
            ["git", "init", "-q", str(tmp)], check=True, capture_output=True
        )
        self.installed = tmp / "hooks" / "pre-push"
        shutil.copy(HOOK, self.installed)
        self.installed.chmod(0o755)
        self.log = tmp / "guard.log"
        self.log.write_text("", encoding="utf-8")

    def guard(self, name: str, label: str, status: int = 0) -> Path:
        """Install one guard that records the ref list and the arguments it was handed."""
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            "#!/usr/bin/env bash\n"
            "refs=$(cat)\n"
            f'echo "{label}:$refs:args=$*" >> {self.log}\n'
            f"exit {status}\n",
            encoding="utf-8",
        )
        path.chmod(0o755)
        return path

    def run(self, stdin: str = REFS) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(self.installed), "origin", "url"],
            input=stdin,
            capture_output=True,
            text=True,
            cwd=str(self.root),
        )

    def seen(self) -> str:
        """The recorded guard calls on one line, so a case can compare against
        a single expected string."""
        return " ".join(self.log.read_text(encoding="utf-8").splitlines())


class Install:
    """A throwaway hooks directory, and the installer pointed at it."""

    def __init__(self, tmp: Path):
        self.hooks = tmp / "hooks"
        self.tmp = tmp

    def occupant(self, text: str, name: str = "pre-push") -> Path:
        path = self.hooks / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        path.chmod(0o755)
        return path

    def link(self, target: Path, name: str = "pre-push") -> Path:
        """Place a symlink in the hooks directory without following it later."""
        self.hooks.mkdir(parents=True, exist_ok=True)
        path = self.hooks / name
        path.symlink_to(target)
        return path

    def run(self) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(INSTALLER), str(self.hooks)],
            capture_output=True,
            text=True,
            cwd=str(REPO),
        )

    def slot(self) -> Path:
        return self.hooks / "pre-push"

    def kept(self) -> Path:
        return self.hooks / "pre-push.local"

    def replaced(self) -> Path:
        return self.hooks / "pre-push.replaced"

    def slot_text(self) -> str:
        return self.slot().read_text(encoding="utf-8")


# ---------------------------------------------------------------- chain side


def test_both_guards_run():
    with tempfile.TemporaryDirectory() as tmp:
        chain = Chain(Path(tmp))
        chain.guard("hooks/pre-push.local", "local")
        chain.guard("scripts/pre_push_hook.sh", "repo")
        proc = chain.run()
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr}"
        expected = (
            "local:refs/heads/dev aaa bbb refs/heads/main ccc ddd:args=origin url "
            "repo:refs/heads/dev aaa bbb refs/heads/main ccc ddd:args=origin url"
        )
        assert chain.seen() == expected, f"replayed stdin mismatch:\n{chain.seen()}"


def test_refusal_stops_chain():
    with tempfile.TemporaryDirectory() as tmp:
        chain = Chain(Path(tmp))
        chain.guard("hooks/pre-push.local", "local", status=1)
        chain.guard("scripts/pre_push_hook.sh", "repo")
        proc = chain.run()
        assert proc.returncode == 1, f"exit {proc.returncode} for a refusing guard"
        assert chain.seen() == (
            "local:refs/heads/dev aaa bbb refs/heads/main ccc ddd:args=origin url"
        ), f"the chain continued past a refusal:\n{chain.seen()}"


def test_no_guards_noop():
    with tempfile.TemporaryDirectory() as tmp:
        chain = Chain(Path(tmp))
        proc = chain.run()
        assert proc.returncode == 0, f"exit {proc.returncode} with no guards installed"
        assert not chain.log.read_text(encoding="utf-8"), "a guard ran that was not there"


def test_dangling_guard_fails():
    with tempfile.TemporaryDirectory() as tmp:
        chain = Chain(Path(tmp))
        (chain.root / "hooks" / "pre-push.local").symlink_to("/nonexistent/guard")
        proc = chain.run()
        assert proc.returncode == 1, f"exit {proc.returncode} for a guard that cannot run"
        assert "symlink with no target" in proc.stderr, (
            f"the refusal did not say why:\n{proc.stderr}"
        )


def test_empty_refs_reach():
    with tempfile.TemporaryDirectory() as tmp:
        chain = Chain(Path(tmp))
        chain.guard("hooks/pre-push.local", "local")
        chain.guard("scripts/pre_push_hook.sh", "repo")
        proc = chain.run(stdin="")
        assert proc.returncode == 0, f"exit {proc.returncode} for an empty ref list"
        assert chain.seen() == "local::args=origin url repo::args=origin url", (
            f"guards skipped on empty stdin:\n{chain.seen()}"
        )


def test_python_guard_runs():
    """The chain runs a guard the way git would, so an interpreter other than
    shell survives being chained."""
    with tempfile.TemporaryDirectory() as tmp:
        chain = Chain(Path(tmp))
        path = chain.root / "hooks" / "pre-push.local"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            "#!/usr/bin/env python3\n"
            "import sys\n"
            f"open({str(chain.log)!r}, 'a').write('python:' + sys.stdin.read())\n",
            encoding="utf-8",
        )
        path.chmod(0o755)
        proc = chain.run()
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr}"
        assert chain.seen() == "python:refs/heads/dev aaa bbb refs/heads/main ccc ddd", (
            f"a non-shell guard did not run as itself:\n{chain.seen()}"
        )


def test_chained_copy_refuses():
    """A chain beside itself runs the repository gate twice and then hands the
    push back to the copy it was called from. The stand-in carries the marker
    and exits instead of being a real copy, because a real one would recurse
    for as long as the case is allowed to run."""
    with tempfile.TemporaryDirectory() as tmp:
        chain = Chain(Path(tmp))
        chain.guard("scripts/pre_push_hook.sh", "repo")
        stand_in = chain.root / "hooks" / "pre-push.local"
        stand_in.write_text(
            f"#!/bin/sh\n# {MARKER}\necho 'should never run' >> {chain.log}\nexit 0\n",
            encoding="utf-8",
        )
        stand_in.chmod(0o755)
        proc = chain.run()
        assert proc.returncode == 1, f"exit {proc.returncode} for a self-chained hook"
        assert "another copy of this chain" in proc.stderr, (
            f"the refusal did not say why:\n{proc.stderr}"
        )
        assert "should-never-run" not in chain.seen(), (
            f"the chain ran the copy instead of refusing:\n{chain.seen()}"
        )


def test_marker_mention_runs():
    """A publish blocker that refuses to push while the chain is missing
    detects it by looking for the marker, so the marker sits in that guard's
    body. Reading the marker out of the whole file refuses every push on that
    machine and misnames the guard as a copy of the chain."""
    with tempfile.TemporaryDirectory() as tmp:
        chain = Chain(Path(tmp))
        path = chain.root / "hooks" / "pre-push.local"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            "#!/usr/bin/env bash\n"
            f"# not a chain, though it checks for one: {MARKER}\n"
            "refs=$(cat)\n"
            f'echo "mentions:$refs" >> {chain.log}\n',
            encoding="utf-8",
        )
        path.chmod(0o755)
        chain.guard("scripts/pre_push_hook.sh", "repo")
        proc = chain.run()
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr}"
        assert chain.seen() == (
            "mentions:refs/heads/dev aaa bbb refs/heads/main ccc ddd "
            "repo:refs/heads/dev aaa bbb refs/heads/main ccc ddd:args=origin url"
        ), f"a guard that mentions the marker was refused or skipped:\n{chain.seen()}"


def installer_single_guard_wrapper() -> str:
    """The wrapper text the installer recognises, read out of the installer
    rather than restated here, so the two sides cannot drift."""
    text = INSTALLER.read_text(encoding="utf-8")
    start = text.index("single_guard_wrapper='") + len("single_guard_wrapper='")
    end = text.index("'\n", start)
    return text[start:end] + "\n"


def test_scripts_agree():
    """Two files agree on two strings and nothing enforces the agreement. The
    marker identifies a chain the installer laid down, and the wrapper text
    identifies the single-guard hook a previous version laid down. An edit
    that moves either side silently reclassifies every install, and the
    direction it fails in is the one that overwrites a guard."""
    hook_text = HOOK.read_text(encoding="utf-8")
    installer_text = INSTALLER.read_text(encoding="utf-8")
    second_line = hook_text.splitlines()[1]
    assert second_line == f"# {MARKER}", (
        f"the hook carries {second_line!r} on the line both scripts read"
    )
    for text, name in ((hook_text, "hook"), (installer_text, "installer")):
        assert f"marker='{MARKER}'" in text, f"the {name} no longer names the marker"
        assert '[ "$line" = "# $marker" ]' in text, (
            f"the {name} no longer reads the marker off one line, so a guard that "
            "mentions it in its body is mistaken for a chain"
        )
    assert installer_single_guard_wrapper() == SINGLE_GUARD_WRAPPER, (
        "the installer no longer recognises the wrapper text this suite lays down"
    )


# ------------------------------------------------------------ installer side
#
# One table drives the recognisable states. Each row names the state, how the
# slot comes to hold it, the exit the installer owes, and the three outcomes
# that make up the contract: whether the chain lands in the slot, which file
# the occupant ends up in, and whether the installer says what it did. A row
# with occupant=None starts from an empty slot.


class Row:
    """One installer state and the outcome the installer owes for it."""

    def __init__(
        self,
        name: str,
        occupant: str | None,
        exit_zero: bool,
        chain_installed: bool,
        occupant_at: str | None,
        says: str | None = None,
    ):
        self.name = name
        self.occupant = occupant
        self.exit_zero = exit_zero
        self.chain_installed = chain_installed
        self.occupant_at = occupant_at
        self.says = says


TABLE = [
    Row("empty slot", None, True, True, None, "pre-commit hook installed"),
    Row(
        "foreign guard",
        "#!/bin/sh\necho 'do not publish the pre-release root commits'\nexit 1\n",
        True,
        True,
        "kept",
    ),
    Row("single-guard wrapper", SINGLE_GUARD_WRAPPER, True, True, "replaced",
        "pre-push.replaced"),
    Row("customised wrapper", CUSTOMISED_WRAPPER, True, True, "kept"),
    Row("padded wrapper", SINGLE_GUARD_WRAPPER + "\n\n", True, True, "replaced"),
    Row("marker-checking guard", MARKER_CHECKING_GUARD, True, True, "kept"),
]


def occupant_into(install: Install, row: Row) -> None:
    """Lay the row's occupant into the slot before the installer runs."""
    install.occupant(row.occupant)


def test_slot_classification():
    """The installer's whole contract is what it does with what it finds. A
    foreign guard is never overwritten — it moves to pre-push.local and runs
    first. The single-guard wrapper is this installer's own previous output
    rather than a guard, so it is set aside as
    pre-push.replaced instead of chained: chained, it would run the repository
    gate a second time on every push. Recognition is exact in both directions,
    which is why a personalised wrapper (checks added above the delegation)
    stays a guard while one that only gained blank lines is still recognised,
    and a publish blocker that carries the chain's marker in its body is kept
    rather than mistaken for the chain itself."""
    for row in TABLE:
        with tempfile.TemporaryDirectory() as tmp:
            install = Install(Path(tmp))
            if row.occupant is not None:
                occupant_into(install, row)
            proc = install.run()
            where = f"{row.name}: "
            assert (proc.returncode == 0) == row.exit_zero, (
                f"{where}exit {proc.returncode}: {proc.stderr}"
            )
            if row.chain_installed:
                assert MARKER in install.slot_text(), (
                    f"{where}the chain was not installed"
                )
                assert install.slot().stat().st_mode & 0o111, (
                    f"{where}the installed chain is not executable"
                )
                assert (install.hooks / "pre-commit").exists(), (
                    f"{where}pre-commit was not installed"
                )
            if row.occupant_at == "kept":
                assert install.kept().read_text(encoding="utf-8") == row.occupant, (
                    f"{where}the guard that held the slot was not preserved beside "
                    "the chain"
                )
                assert not install.replaced().exists(), (
                    f"{where}a guard was set aside as though it were the "
                    "installer's own output"
                )
            elif row.occupant_at == "replaced":
                assert not install.kept().exists(), (
                    f"{where}the single-guard wrapper was kept beside the chain, so the "
                    "repository gate would run twice per push"
                )
                assert install.replaced().read_text(encoding="utf-8") == (
                    row.occupant
                ), f"{where}the wrapper it replaced was not kept anywhere"
            elif row.occupant_at is None:
                assert not install.kept().exists(), (
                    f"{where}a local guard appeared where the row expects none"
                )
            if row.says:
                assert row.says in proc.stdout, (
                    f"{where}the message does not say what it did:\n{proc.stdout}"
                )


def test_symlinked_guard_kept():
    """A linked guard keeps pointing at the file that updates it: the move to
    pre-push.local does not follow the link, so the guard stays reachable from
    its own source."""
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        source = Path(tmp) / "external-guard.sh"
        source.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        source.chmod(0o755)
        install.link(source)
        proc = install.run()
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr}"
        assert install.kept().is_symlink(), "the kept guard was followed, not moved"
        assert install.kept().resolve() == source.resolve(), (
            "the kept guard no longer points at its own source"
        )


def test_link_at_chain():
    """A hooks directory that links the slot straight at the tracked chain is
    already installed, and better installed than a copy would be: the link
    picks up every later change to the file it reaches."""
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.link(HOOK)
        proc = install.run()
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr}"
        assert install.slot().is_symlink(), "the link was replaced by a copy"
        assert install.slot().resolve() == HOOK.resolve(), (
            "the link no longer reaches the chain"
        )
        assert not install.kept().exists(), (
            "an installed chain was moved aside as though it were a guard"
        )
        assert "keeping the symlink" in proc.stdout, (
            f"the install did not say what it decided:\n{proc.stdout}"
        )


def test_link_at_wrapper():
    """A slot linked at the single-guard wrapper is moved aside as a guard,
    because a link is somebody's and is never written through. Chained from
    there it would delegate to the repository gate the chain already runs, so
    the move that deposited it is followed by the same recognition."""
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        wrapper = Path(tmp) / "old-wrapper.sh"
        wrapper.write_text(SINGLE_GUARD_WRAPPER, encoding="utf-8")
        wrapper.chmod(0o755)
        install.link(wrapper)
        proc = install.run()
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr}"
        assert MARKER in install.slot_text(), "the chain was not installed"
        assert not install.kept().exists(), (
            "the linked wrapper stayed beside the chain, so the repository gate "
            "would run twice per push"
        )
        assert wrapper.read_text(encoding="utf-8") == SINGLE_GUARD_WRAPPER, (
            "the file the link reached was written through"
        )


def test_wrapper_beside_chain():
    """A single-guard wrapper left at pre-push.local by an earlier install
    delegates to the repository gate, which the chain runs itself. Left there
    it doubles the gate on every push, so it is set aside instead of chained."""
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.occupant(SINGLE_GUARD_WRAPPER, name="pre-push.local")
        proc = install.run()
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr}"
        assert MARKER in install.slot_text(), "the chain was not installed"
        assert not install.kept().exists(), (
            "the single-guard wrapper stayed beside the chain, so the repository gate "
            "would run twice per push"
        )
        assert install.replaced().read_text(encoding="utf-8") == SINGLE_GUARD_WRAPPER, (
            "the wrapper that was set aside was not kept"
        )


def test_wrapper_beside_foreign():
    """A single-guard wrapper an earlier install left beside the slot is not a
    second guard. When a foreign guard later takes the slot, the install must
    still finish: the wrapper is set aside as the installer's own previous
    output and the guard is kept beside the chain. Without the first look at
    what sits beside the slot, the install refuses and calls its own
    wrapper a second guard."""
    foreign = "#!/bin/sh\nexit 1\n"
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.occupant(foreign)
        install.occupant(SINGLE_GUARD_WRAPPER, name="pre-push.local")
        proc = install.run()
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr}"
        assert MARKER in install.slot_text(), "the chain was not installed"
        assert install.kept().read_text(encoding="utf-8") == foreign, (
            "the foreign guard did not end up beside the chain"
        )
        assert install.replaced().read_text(encoding="utf-8") == SINGLE_GUARD_WRAPPER, (
            "the wrapper that sat beside the slot was not set aside"
        )


def test_directory_refused():
    """A directory is not a guard. Installing a chain beside it succeeds and
    then fails every push with an interpreter error, so the refusal belongs at
    install time, where the reader can still do something about it."""
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.hooks.mkdir(parents=True, exist_ok=True)
        (install.hooks / "pre-push").mkdir()
        proc = install.run()
        assert proc.returncode == 1, f"exit {proc.returncode} for a directory in the slot"
        assert install.slot().is_dir(), "the directory was moved or removed"
        assert "is a directory, not a guard" in proc.stderr, (
            f"the refusal did not say why:\n{proc.stderr}"
        )
        assert "hook installed" not in proc.stdout, (
            f"the installer reported success over a slot it refused:\n{proc.stdout}"
        )


def test_two_guards_refused():
    foreign = "#!/bin/sh\nexit 1\n"
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.occupant(foreign)
        install.occupant("#!/bin/sh\nexit 0\n", name="pre-push.local")
        proc = install.run()
        assert proc.returncode == 1, f"exit {proc.returncode} with two guards in play"
        assert install.slot_text() == foreign, "a refused install changed the slot"
        assert "already holds one" in proc.stderr, (
            f"the refusal did not say why:\n{proc.stderr}"
        )


def test_chain_aside_refused():
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        chain_copy = HOOK.read_text(encoding="utf-8")
        install.occupant(chain_copy, name="pre-push.local")
        proc = install.run()
        assert proc.returncode == 1, f"exit {proc.returncode} for a chained copy"
        assert not install.slot().exists(), "a refused install wrote to the slot"
        assert "is itself an installed chain" in proc.stderr, (
            f"the refusal did not say why:\n{proc.stderr}"
        )


def test_reinstall_leaves_guard():
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.occupant("#!/bin/sh\nexit 0\n")
        assert install.run().returncode == 0, "the first install failed"
        first = install.slot_text()
        proc = install.run()
        assert proc.returncode == 0, f"reinstall exit {proc.returncode}: {proc.stderr}"
        assert install.slot_text() == first, "a reinstall changed the installed chain"
        assert install.kept().read_text(encoding="utf-8") == "#!/bin/sh\nexit 0\n", (
            "a reinstall disturbed the guard it had already moved aside"
        )


def test_removal_hint_guard():
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.occupant("#!/bin/sh\nexit 0\n")
        proc = install.run()
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr}"
        assert "pre-push.local" in proc.stdout, (
            "the hint tells the reader to remove the chain and never mentions "
            f"the guard it moved aside:\n{proc.stdout}"
        )


def test_failed_move_reports():
    """The lie this pins is smaller than a crash: an installer that reports it
    moved a guard aside when the move never happened. The slot still holds the
    guard, nothing is chained in front of it, and the machine reads as gated
    while the repository gate is not installed."""
    foreign = "#!/bin/sh\nexit 1\n"
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.occupant(foreign)
        install.hooks.chmod(0o555)
        try:
            proc = install.run()
        finally:
            install.hooks.chmod(0o755)
        assert proc.returncode != 0, (
            f"exit {proc.returncode} for a move that could not happen"
        )
        assert "kept the existing pre-push" not in proc.stdout, (
            f"the installer claimed a move it did not make:\n{proc.stdout}"
        )
        assert install.slot_text() == foreign, "the guard in the slot changed"
        assert not install.kept().exists(), "a local guard appeared where the move failed"


def test_precommit_slot_fails():
    """The pre-push copy is checked after it lands; the pre-commit write has no
    such check, so aborting on a failing step is the only thing standing between
    a half-finished install and a message saying it finished."""
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.hooks.mkdir(parents=True)
        (install.hooks / "pre-commit").mkdir()
        proc = install.run()
        assert proc.returncode != 0, (
            f"exit {proc.returncode} for a pre-commit slot that refuses a file"
        )
        assert "pre-commit hook installed" not in proc.stdout, (
            f"the installer reported a pre-commit hook over a directory:\n{proc.stdout}"
        )


def test_failed_install_reports():
    """The failure mode worth pinning is not a crash but a lie: an installer
    that cannot write the slot and still prints that it installed one leaves
    every later push running no gate at all while reporting success."""
    with tempfile.TemporaryDirectory() as tmp:
        install = Install(Path(tmp))
        install.hooks.mkdir(parents=True)
        install.hooks.chmod(0o555)
        try:
            proc = install.run()
        finally:
            install.hooks.chmod(0o755)
        assert proc.returncode != 0, f"exit {proc.returncode} for an unwritable slot"
        assert not install.slot().exists(), "a slot appeared in a directory that refuses writes"
        assert "pre-push hook installed" not in proc.stdout, (
            f"the installer reported success over an empty slot:\n{proc.stdout}"
        )


if __name__ == "__main__":
    from test_runner import run

    sys.exit(run("pre-push-chain", dict(globals())))
