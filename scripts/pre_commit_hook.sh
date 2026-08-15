#!/usr/bin/env bash
# Pre-commit gate: runs make check, narrowed by staged file type so a
# docs-only commit does not pay for the full unit suite.
#
# Routing:
#   .rs staged      → make check (clippy + tests + diff-cov + all gates)
#   scripts/**.py   → script-tests + no-cjk (the .py-relevant gates)
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
  python3 scripts/test_hook_rust.py
  python3 scripts/test_cov_lcov.py
  python3 scripts/test_flat_prefix.py
  python3 scripts/test_diff_cov.py
  python3 scripts/test_stderr_gate.py
  python3 scripts/check_no_cjk.py
elif $has_md; then
  if [ -f scripts/check_sdd_naming.py ]; then
    python3 scripts/check_sdd_naming.py
  fi
fi
