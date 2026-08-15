#!/usr/bin/env bash
# Pre-push hook: the last gate before the remote sees the change.
#
# A push is a publish action -- published history is immutable (rewriting
# already-pushed history is an open-source taboo), so anything that crosses
# this gate is final. Two checks run:
#
# 1. make check-full (the full unit + integration suite). make check (the
#    unit gate) runs before every commit; this runs the heavier integration suite
#    (cross-decode, drop-in, dual-transport equivalence, control-lease)
#    before a push, so the most valuable invariants of the refactor cannot
#    regress silently.
#
# 2. Re-scan every commit in the about-to-publish range (@{u}..HEAD -- the
#    local commits not yet on the upstream) through the same style lint the
#    commit-msg hook runs. Commit-msg catches each commit as written; this
#    catches the whole range once more, so a commit that slipped past a
#    disabled/missing local wordlist cannot ship. This is the non-negotiable
#    gate; the commit-msg check is advisory by comparison.
#
# 3. A human-eye reminder: the machine scans for wordlist matches; the author
#    eyeballs tone, voice, and information density before confirming the push.
#
# Install with `make setup-hooks`.
set -e
unset GIT_DIR GIT_INDEX_FILE GIT_WORK_TREE GIT_PREFIX GIT_OBJECT_DIRECTORY

echo "pre-push: running make check-full (lint + unit + integration + coverage)..."
make check-full
echo "pre-push: gate green."

repo=$(git rev-parse --show-toplevel 2>/dev/null || pwd)

# Re-scan the about-to-publish range. @{u}..HEAD = local commits not yet on
# the upstream. No upstream = the first push to this remote: the entire
# local history is being published at once, so the per-commit scan is
# skipped (it would flag the legacy history, which stays as harmless
# internal records). The author reviews the whole range by eye on a first
# push -- this is the curated release point. After the upstream is set,
# only genuinely new commits are scanned, so the discipline enforces
# forward from here without rewriting the past.
if git rev-parse --abbrev-ref --symbolic-full-name '@{u}' >/dev/null 2>&1; then
    range='@{u}..HEAD'
    count=$(git rev-list --count "$range" 2>/dev/null || echo 0)
    if [ "$count" -gt 0 ]; then
        echo "pre-push: re-scanning $count commit(s) in $range through the style lint..."
        while read -r sha; do
            tmp=$(mktemp)
            git log --format='%B' -n 1 "$sha" > "$tmp"
            if ! bash "$repo/scripts/commit_msg_check.sh" "$tmp" >/dev/null 2>&1; then
                cat >&2 <<EOF
ERROR: commit $sha fails the style lint on the pre-push gate.
Published history is immutable; fix it here, in place:
  git rebase -i reword $sha
then re-attempt the push. (structural checks + any installed local wordlist)
EOF
                rm -f "$tmp"
                exit 1
            fi
            rm -f "$tmp"
        done < <(git rev-list "$range")
        echo "pre-push: range clean."
    fi
else
    cat >&2 <<'EOF'
pre-push WARN: no upstream configured for this branch -- first push to this
remote. The per-commit style-lint scan is SKIPPED (it would flag the legacy
history). You are publishing the full local history at once; review it by
eye now -- this is the curated release point. After this push sets the
upstream, subsequent pushes scan only new commits, enforcing the discipline
forward without rewriting the past.
EOF
fi

cat <<'EOF'

pre-push: HUMAN EYE REQUIRED before confirming the push.
  - tone: factual + neutral, no self-referential process voice
  - voice: imperative subject, body explains why (diff says what)
  - no internal process references (milestone IDs / file:line / before-after)
Published history is immutable -- this is the final gate.

EOF
