#!/usr/bin/env bash
# Activate a new worktree: copy the gitignored settings and the wordlist
# from the main checkout, then the user re-enters the worktree to reload.
# Tracked scripts (hook_rust.py etc.) are already in the worktree checkout.
set -euo pipefail
MAIN_CHECKOUT="/Users/von/workspace/hicoder"

# Copy settings.json (gitignored, per-worktree)
mkdir -p .claude
cp "$MAIN_CHECKOUT/.claude/settings.json" .claude/settings.json

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
