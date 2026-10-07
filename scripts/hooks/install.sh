#!/usr/bin/env bash
# Install the repository's git hooks.
#
# The hooks directory belongs to the repository rather than to one worktree,
# so every checkout and every worktree shares a single set of hooks. Two slots
# are written:
#
#   pre-commit  a wrapper that delegates to scripts/pre_commit_hook.sh, which
#               filters the gate by staged file type.
#   pre-push    scripts/hooks/pre-push, a chain that runs a guard already
#               holding the slot and then the repository gate.
#
# An occupant of the pre-push slot is somebody's guard and is never destroyed.
# Anything this script does not recognise as its own output is moved beside the
# chain as pre-push.local, where the chain runs it first, and a move does not
# follow a symlink, so a linked guard keeps pointing at the source that updates
# it.
#
# Two occupants are this script's own earlier output rather than guards, and
# are set aside as pre-push.replaced instead of being chained. Both are
# recognised by their whole content, never by a substring: a wrapper a
# developer added their own checks to is no longer that output and stays a
# guard. One is a chain this script installed, which carries a marker on its
# second line. The other is the single-guard wrapper, which delegates the
# whole push to the repository gate the chain already runs, so chaining it
# would run that gate twice on every push. Overwriting pre-push.replaced loses
# nothing, because only a file already equal to the known wrapper can be moved
# there.
#
# Every step that can fail is checked. A hook slot that looks installed and is
# not is worse than one that is visibly empty: the push reports success having
# run no gate at all.
#
# Usage: install.sh [hooks-dir]. The argument exists for tests, which point it
# at a throwaway directory; the default is the shared hooks directory git
# reports for the current repository.
set -euo pipefail

repo=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
hooks_dir=${1:-$(git rev-parse --git-common-dir)/hooks}

marker='houyicoder installed pre-push'
chain_source=$repo/scripts/hooks/pre-push
slot=$hooks_dir/pre-push
kept=$hooks_dir/pre-push.local
replaced=$hooks_dir/pre-push.replaced

# The single-guard wrapper: all it does is delegate the whole push to the
# repository gate, without the local guard the chain runs first. A previous
# version installed it into the slot; this one installs the chain in its
# place. Read through a command substitution, which strips trailing newlines,
# so one more blank line at the end still matches, and compared as a whole, so
# one more line of somebody's own checking does not.
single_guard_wrapper='#!/usr/bin/env bash
set -euo pipefail
repo=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
[ -f "$repo/scripts/pre_push_hook.sh" ] || exit 0
exec bash "$repo/scripts/pre_push_hook.sh"'

# The marker is read off one line rather than out of the whole file. A publish
# blocker that detects the chain by looking for its marker carries the same
# string in its body, and a whole-file match would classify that guard as the
# chain and overwrite it.
carries_marker() {
    local file=$1 line
    [ -f "$file" ] || return 1
    line=$(sed -n '2p' "$file" 2>/dev/null) || return 1
    [ "$line" = "# $marker" ]
}

is_single_guard_wrapper() {
    local file=$1 content
    [ -f "$file" ] || return 1
    content=$(cat "$file" 2>/dev/null) || return 1
    [ "$content" = "$single_guard_wrapper" ]
}

# The single-guard wrapper is not a guard, so it may not sit where a kept
# guard belongs: chained from there it would run the repository gate a second
# time on every push. Called before classification, so such a file is not
# mistaken for a second guard and refused, and again after the moves below,
# which can deposit one there.
set_aside_single_guard() {
    if is_single_guard_wrapper "$kept"; then
        mv "$kept" "$replaced"
        echo "moved the single-guard wrapper out of pre-push.local — the chain"
        echo "  runs the repository gate it delegated to"
    fi
}

mkdir -p "$hooks_dir"

if [ -d "$slot" ]; then
    echo "ERROR: $slot is a directory, not a guard. Remove it, or put the" >&2
    echo "  guard it was meant to hold there, then re-run." >&2
    echo "  Nothing was installed." >&2
    exit 1
fi

set_aside_single_guard

# Classify whatever holds the slot before touching it, so a refusal leaves the
# machine exactly as it was found.
if [ -L "$slot" ] || [ -e "$slot" ]; then
    if carries_marker "$slot"; then
        action=reinstall
    elif [ ! -L "$slot" ] && is_single_guard_wrapper "$slot"; then
        action=replace
    elif [ -e "$kept" ] || [ -L "$kept" ]; then
        echo "ERROR: $slot holds a guard this script did not install, and" >&2
        echo "  $kept already holds one. Decide which of the two is" >&2
        echo "  this machine's guard, remove the other, then re-run." >&2
        exit 1
    else
        action=keep
    fi
else
    action=install
fi

if [ "$action" = keep ]; then
    mv "$slot" "$kept"
    if [ -e "$slot" ] || [ -L "$slot" ]; then
        echo "ERROR: moving the guard aside left $slot occupied, so the chain" >&2
        echo "  was not installed and the guard was not chained." >&2
        exit 1
    fi
    if [ ! -e "$kept" ] && [ ! -L "$kept" ]; then
        echo "ERROR: the guard that held $slot is now at neither $slot nor" >&2
        echo "  $kept. Nothing was installed." >&2
        exit 1
    fi
    echo "kept the existing pre-push as pre-push.local — it runs first"
fi

if [ "$action" = replace ]; then
    mv "$slot" "$replaced"
    echo "replaced the single-guard wrapper, now at pre-push.replaced"
    echo "  — the chain runs the repository gate it delegated to"
fi

set_aside_single_guard

# A chain beside another copy of itself would run the repository gate twice,
# and the second copy would chain the first back, so neither may be installed.
if carries_marker "$kept"; then
    echo "ERROR: $kept is itself an installed chain. Restore the guard that" >&2
    echo "  slot held, or remove the copy, then re-run." >&2
    exit 1
fi

# A slot already symlinked at a chain needs no copy. The link keeps reaching
# the tracked file, so it picks up every later change to it, which a copy
# would stop doing.
if [ "$action" = reinstall ] && [ -L "$slot" ]; then
    echo "keeping the symlink at $slot — it already reaches a chain, and"
    echo "  updates from there"
else
    cp "$chain_source" "$slot"
    chmod +x "$slot"
fi
if [ ! -s "$slot" ]; then
    echo "ERROR: $slot is empty after the copy, so a push would run no gate." >&2
    exit 1
fi
if ! carries_marker "$slot"; then
    echo "ERROR: $slot does not carry the chain marker after the copy, so a" >&2
    echo "  later setup-hooks would treat it as a foreign guard." >&2
    exit 1
fi

printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'repo=$(git rev-parse --show-toplevel 2>/dev/null || pwd)' \
    '[ -f "$repo/scripts/pre_commit_hook.sh" ] || exit 0' \
    'exec bash "$repo/scripts/pre_commit_hook.sh"' \
    > "$hooks_dir/pre-commit"
chmod +x "$hooks_dir/pre-commit"
chmod +x "$repo/scripts/pre_push_hook.sh" "$repo/scripts/pre_commit_hook.sh" \
    "$chain_source"

echo "pre-commit hook installed (make check, filtered by staged file type,"
echo "  and a no-op when the repository carries no gate script)."
echo "pre-push hook installed (a local guard first if one was kept, then the"
echo "  coverage gate plus the per-commit re-scan of the about-to-publish range,"
echo "  and a no-op when the repository carries no gate script)."
echo "Remove: rm $hooks_dir/pre-commit $slot"
echo "  A guard this script moved aside is at $kept. Moving it back to $slot"
echo "  restores the machine to the state setup-hooks found it in."
