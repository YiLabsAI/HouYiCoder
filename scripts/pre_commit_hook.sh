#!/usr/bin/env bash
# Pre-commit gate: runs make check, narrowed by staged file type so a
# docs-only commit does not pay for the full unit suite.
#
# Routing:
#   .rs staged      → make check (clippy + tests + diff-cov + all gates)
#   scripts/**.py   → the check's Python gates (CHECK_SKIP_RUST=1)
#   docs/**/*.md    → sdd-naming (task-ID gate; .md is exempt from no-cjk)
#   nothing staged  → skip (deletion-only or empty commit)
#
# WIP commits bypass with --no-verify.
set -euo pipefail
unset GIT_DIR GIT_INDEX_FILE GIT_WORK_TREE GIT_PREFIX GIT_OBJECT_DIRECTORY

staged=$(git diff --cached --name-only --diff-filter=ACM)
[ -z "$staged" ] && exit 0

has_rs=false
has_py=false
has_md=false
while IFS= read -r f; do
  case "$f" in
    *.rs)            has_rs=true ;;
    scripts/*.py)    has_py=true ;;
    *.md|docs/*)     has_md=true ;;
  esac
done <<< "$staged"

if $has_rs; then
  exec make check
elif $has_py; then
  # The same Python gates the check runs, without the Rust steps. Listing
  # them here again let the two registries drift: a test added to the check
  # was not run at commit time.
  CHECK_SKIP_RUST=1 exec scripts/check_code.sh
elif $has_md; then
  if [ -f scripts/check_sdd_naming.py ]; then
    python3 scripts/check_sdd_naming.py
  fi
fi
