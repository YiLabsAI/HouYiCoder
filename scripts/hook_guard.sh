#!/usr/bin/env bash
# Hook guard: runs an internal hook if it exists on disk; no-ops if absent.
# Resolution order: the current tree first ($CLAUDE_PROJECT_DIR/scripts),
# then the MAIN checkout (via the git common dir) - a worktree checkout does
# not carry gitignored internal hooks, so without the fallback the internal
# rules would silently no-op for all worktree-first development. External
# clones have neither copy and the guard still no-ops rather than erroring
# on a missing script.
#
# Usage: bash scripts/hook_guard.sh <hook_script.py> [args...]
# stdin (Claude Code hook JSON) is passed through to the inner script.
PROJ="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
SCRIPT="$PROJ/scripts/$1"
if [ ! -f "$SCRIPT" ]; then
  MAIN=$(git -C "$PROJ" rev-parse --git-common-dir 2>/dev/null) || MAIN=""
  if [ -n "$MAIN" ]; then
    case "$MAIN" in
      /*) ;;
      *) MAIN="$PROJ/$MAIN" ;;
    esac
    CANDIDATE="$(dirname "$MAIN")/scripts/$1"
    [ -f "$CANDIDATE" ] && SCRIPT="$CANDIDATE"
  fi
fi
shift
if [ -f "$SCRIPT" ]; then
    exec python3 "$SCRIPT" "$@"
fi
exit 0
