## Summary

<!-- What this changes and why. One or two sentences on the problem, then the
     approach. Link the issue it closes (e.g. Closes #12) if any. -->

## Test plan

<!-- How this was verified. Name the tests added/changed and what they cover. -->

## Breaking changes

<!-- None, or describe what breaks and the migration path. -->

## Checklist

- [ ] `make check` passes (pre-commit gate: fmt, clippy, comments, naming, file-size, dep-graph, unit tests, diff-cov)
- [ ] `make verify` passes (unit tests + unit coverage + the ignored suite)
      <!-- The non-ignored integration suite (tests/ binaries) runs in CI only,
           via run_tests.py --full; no local make target runs it yet. -->
- [ ] UI changes include a screenshot / PTY capture
- [ ] Commit messages follow the conventional format (type(scope): subject ≤72)
- [ ] No internal doc/codename references introduced
