#!/usr/bin/env bash
# Hook guard: runs an internal hook if it exists on disk; no-ops if absent.
# External clones and fresh worktrees have no internal hooks — the guard
# silently allows the write rather than erroring on a missing script.
#
# Usage: bash scripts/hook_guard.sh <hook_script.py> [args...]
# stdin (Claude Code hook JSON) is passed through to the inner script.
SCRIPT="$CLAUDE_PROJECT_DIR/scripts/$1"
shift
if [ -f "$SCRIPT" ]; then
    exec python3 "$SCRIPT" "$@"
fi
exit 0
