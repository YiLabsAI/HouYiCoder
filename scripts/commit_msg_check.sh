#!/usr/bin/env bash
# Commit-msg hook. Two layers:
#   1. Structural (here): conventional prefix + 72-char subject.
#   2. Content (commit_msg_lint.py): codename rules shared with the
#      .rs comment gate via rules/comments.py.
# An optional gitignored wordlist (.commit-lint-words) adds local
# commit-only style patterns; absent, that block skips.
# Install via make setup-hooks.
set -euo pipefail


msg_file="$1"
repo=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
first_line=$(head -1 "$msg_file")

# Must match: type(scope?): subject
pattern='^(feat|fix|docs|refactor|test|chore|style|perf|ci|build)(\(.+\))?: .+'

if ! echo "$first_line" | grep -qE "$pattern"; then
    cat >&2 <<'EOF'
ERROR: commit message must follow conventional commits:
  feat: / fix: / docs: / refactor: / test: / chore: / style: / perf: / ci: / build:
  optional scope: feat(tui): ...
  imperative mood: "add" not "added"
  first line <= 72 chars, ASCII only (English)
EOF
    echo "  got: $first_line" >&2
    exit 1
fi

# First line <= 72 chars
len=${#first_line}
if [ "$len" -gt 72 ]; then
    echo "ERROR: commit first line must be <= 72 chars (got $len)" >&2
    echo "  $first_line" >&2
    exit 1
fi

# Body lines (after the subject) wrap at 72 (AGENTS.md "Body: wrap at 72").
# The subject check above covers line 1; this catches unwrapped body prose.
lineno=0
while IFS= read -r line; do
    lineno=$((lineno + 1))
    [ "$lineno" -eq 1 ] && continue
    if [ "${#line}" -gt 72 ]; then
        echo "ERROR: commit body line $lineno exceeds 72 chars (got ${#line}); wrap the body at 72 (AGENTS.md)." >&2
        echo "  $line" >&2
        exit 1
    fi
done < "$msg_file"

# Content lint: reuse the write-time gate's codename rules (codename +
# acceptance-code patterns + ASCII subject). See the python script
# header for the complementary split.
if [ -f "$repo/scripts/commit_msg_lint.py" ]; then
    python3 "$repo/scripts/commit_msg_lint.py" "$msg_file"
fi

# Optional local extra: a gitignored wordlist of commit-only style
# patterns the shared codename rules do not cover. The shared rules
# (codenames + acceptance codes) live in the python lint above so the
# wordlist stays complementary, not duplicative.
words="${COMMIT_LINT_WORDS:-$repo/.commit-lint-words}"
if [ -f "$words" ]; then
    active=$(grep -Ev '^[[:space:]]*(#.*)?$' "$words" 2>/dev/null || true)
    if [ -n "$active" ] && printf '%s\n' "$active" | grep -Eqf- "$msg_file"; then
        cat >&2 <<EOF
ERROR: commit message matches a local style-lint pattern.
The shared rules (codename + acceptance-code patterns) are in commit_msg_lint.py;
this block fires from the gitignored local wordlist for commit-only extras.
Reword and re-commit.
Wordlist: $words
EOF
        printf '%s\n' "$active" | grep -Eohf- "$msg_file" >&2 || true
        exit 1
    fi
fi
