#!/usr/bin/env bash
# Activate hooks in a new worktree: copy gitignored hook scripts + settings
# from the main checkout, then the user re-enters the worktree to reload.
# Tracked scripts (hook_rust.py etc.) are already in the worktree checkout.
set -euo pipefail
MAIN_CHECKOUT="/Users/von/workspace/hicoder"

# Copy settings.json (gitignored, per-worktree)
mkdir -p .claude
cp "$MAIN_CHECKOUT/.claude/settings.json" .claude/settings.json

# Copy gitignored hook scripts + the product-name wordlist (in scripts/ but
# not tracked). The wordlist feeds the .rs-comment product-name arm; without
# it the arm is inert and product names leak past the gate.
for f in hook_commit_gate.py hook_sdd_naming.py hook_doc_sync.py .rs-comment-products; do
  src="$MAIN_CHECKOUT/scripts/$f"
  [ -f "$src" ] && cp "$src" "scripts/$f" && echo "copied scripts/$f"
done

echo "Done. ExitWorktree -> EnterWorktree to reload hooks."
