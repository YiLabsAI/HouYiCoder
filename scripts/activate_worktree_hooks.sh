#!/usr/bin/env bash
# Activate a new worktree: copy the gitignored settings and the wordlist
# from the main checkout, then the user re-enters the worktree to reload.
# Tracked scripts (hook_rust.py etc.) are already in the worktree checkout.
set -euo pipefail
MAIN_CHECKOUT="/Users/von/workspace/hicoder"
WORKTREE="$(pwd)"

# Write-time hooks are configured per settings file, and this product reads
# two of them: its own .houyicoder/settings.json plus the harness
# .claude/settings.json. Which one a session obeys is decided at assembly
# from its workspace root, so both are laid down rather than assuming one
# surface. A settings file the main checkout does not have is skipped, not an
# error: a checkout without one is a checkout that never configured that
# surface.
lay_down() {
  local rel="$1"
  local src="$MAIN_CHECKOUT/$rel"
  local dst="$WORKTREE/$rel"
  if [ ! -f "$src" ]; then
    echo "skipped $rel (absent in the main checkout)"
    return 0
  fi
  mkdir -p "$(dirname "$dst")"
  cp "$src" "$dst"
  # The copy can be refused (the settings paths are fenced) and cp reports
  # that only through its exit status, so the landed bytes are what says
  # whether the hook is configured. An empty or missing file would leave the
  # hook silently inert, which is the failure this script exists to prevent.
  if [ ! -s "$dst" ]; then
    echo "FAILED to lay down $rel" >&2
    return 1
  fi
  # A copied command that names the main checkout would run the gate against
  # the wrong tree, so the worktree path is substituted in.
  if grep -q "$MAIN_CHECKOUT" "$dst" 2>/dev/null; then
    sed -i.bak "s|$MAIN_CHECKOUT|$WORKTREE|g" "$dst" && rm -f "$dst.bak"
  fi
  echo "laid down $rel"
}

lay_down ".houyicoder/settings.json"
lay_down ".claude/settings.json"

# Copy the product-name wordlist (in scripts/ but not tracked). It feeds the
# .rs-comment product-name check; without the wordlist that check is inert and
# product names leak past the gate.
WORDLIST="$MAIN_CHECKOUT/scripts/.rs-comment-products"
[ -f "$WORDLIST" ] && cp "$WORDLIST" scripts/.rs-comment-products &&
  echo "copied the product-name wordlist"

# The commit gate is deliberately not copied. The guard resolves a hook from
# the current tree first, so a copy here would run for a session working in
# this tree and drift from the main checkout's copy at every later edit, the
# same tree running two versions of one gate. With no copy the guard falls
# back to the main checkout through the git common dir, which keeps one
# definition of the gate for every tree, and the gate's meta-test imports
# that same module.

echo "Done. ExitWorktree -> EnterWorktree to reload hooks."