# Scripts

Quality gates and build helpers invoked by `make check` and `make verify`.
These two make targets are the canonical entry points — the scripts here
are their implementation, not a CLI to drive directly.

## Two tiers

- **`make check`** — the commit gate. Fast (target ~10s warm). Runs on
  every change before a commit. Structural + unit-level: format, clippy,
  comment style, no-CJK-in-source, naming, file-size, dependency graph,
  stderr hygiene, and the `--lib` unit suite with diff coverage. Must be
  green to commit.
- **`make verify`** — the verify gate. Slower (runs the full workspace
  test suite including integration and `#[ignore]` live-server tests,
  coverage ratchets, and doc-stale reports). Run before declaring a
  change done or opening a PR.

Both are plain `python3` + `cargo` invocations wired in `Makefile` and
`scripts/check_code.sh`; no special runtime is required.

## Layout

`scripts/` is organized by role, not by tier:

- `check_*.py` — the gate checks (run by `make check` / `make verify`).
- `test_*.py` — meta-tests for the gates (the "gate of the gate").
- `rules/` — shared rule modules imported by both the gate checks and the
  write-time hooks, so write-time and check-time cannot drift.
- `hook_*.py` + `hook_guard.sh` — write-time PreToolUse/PostToolUse
  intercepts that block a bad edit before it lands.
- `*.sh` — shell helpers: the gate runner, the test dispatcher,
  tool-install guards, and the git hook wrappers.
- misc — the unit runner, the coverage helper, the structure report.

## Gate checks (shipped)

These scripts are part of the repository and run for every contributor:

| Script | Concern |
|---|---|
| `check_rust_naming.py` | Test-fn `test_` prefix, segment/length caps, jargon, vague suffixes, flat-prefix module pairs |
| `check_rs_comments.py` | .rs comment style — the 6-arm detector: CJK, backtick, codename, doc-ref, own-crate name, comparison framing |
| `check_file_size.py` | Per-file line-count ratchet (prod vs test thresholds, continuous excess) |
| `check_dep_graph.py` | Crate-to-crate dependency layering whitelist |
| `check_stderr.py` | No stray console writes in runtime code |
| `check_module_dead_code.py` | Dead-code ratchet (new `#[allow(dead_code)]` flagged) |
| `check_comment_block_length.py` | In-function comment-block size ratchet (the verbose-comment smell) |
| `check_struct_fields.py` | Struct field-count ratchet (God-struct growth blocks) |
| `check_app_mut_borrows.py` | Composition-root borrow hygiene |
| `check_gatectx_field_names.py` | Field-name stability in the shared decision context |
| `check_diff_coverage.py` | New executable lines ≥85% covered (diff-cov floor) |
| `check_no_cjk.py` | Source comments are English (no CJK in `.rs`) |
| `cov_lcov.py` | Shared lcov stale-mapping detector used by the coverage gates |
| `run_tests.py` | The `--lib` unit runner with budget + crate filtering |
| `commit_msg_lint.py` | Commit-message structure (conventional prefix, ≤72) and prose cleanliness |

## Meta-tests (gate of the gate)

Each `test_*.py` pins the behavior of a gate or shared rule so a future
widening of a regex or a ratchet-math drift is caught before it silently
re-opens a gap. They run as the `script-tests` step of `make check`
(~0.3s; a gate is the machine backstop for a rule the model applies
unreliably, so the gate itself is guarded).

| Meta-test | Pins |
|---|---|
| `test_hook_rust.py` | The codename + comparison-framing patterns (`rules.comments`) — both directions: must flag + must pass |
| `test_file_size.py` | The test/prod split + the excess ratchet (growth blocks, improvement passes, ceiling green) |
| `test_diff_cov.py` | The diff-coverage new-line accounting |
| `test_cov_lcov.py` | The shared lcov stale-mapping detector |
| `test_flat_prefix.py` | The flat-prefix module gate (naming rule 8) |
| `test_struct_fields.py` | The struct field-count strict-pin |
| `test_module_dead_code.py` | The dead-code strict-pin |
| `test_comment_block_length.py` | The comment-block indented-only narrowing |
| `test_app_mut_borrows.py` | The composition-root borrow strict-pin |
| `test_app_coupling.py` | The App-coupling measurement (default-include / explicit-exclude) |
| `test_stderr_gate.py` | The console-write gate |

## Shell helpers

| Script | Role |
|---|---|
| `check_code.sh` | The `make check` runner — fail-fast, stops at the first red gate |
| `test.sh` | The `make test` dispatcher — unit / integration / ui / live categories |
| `quick_check.sh` | Dev fast path — fmt + clippy only, no tests |
| `check_coverage.sh` | Workspace unit-coverage total, one global threshold — `make check-full` and CI (cargo-llvm-cov; opt-in locally until the tool is installed, installed explicitly in CI) |
| `ensure_nextest.sh` | Installs nextest if absent, before the test runner needs it |
| `ensure_jscpd.sh` | Installs jscpd if absent, before the duplicate-code report needs it |
| `pre_commit_hook.sh` | Git `pre-commit` wrapper — runs `make check` |
| `pre_push_hook.sh` | Git `pre-push` wrapper — runs the structural + content checks |

## Write-time hooks

`hook_rust.py` is a tracked PreToolUse intercept: on an Edit/Write to a
`.rs` file it runs the same `rules.comments` 6-arm detector as
`check_rs_comments.py`, so a comparison-framing or codename comment is
blocked at write time, not just at the next `make check`.

`hook_guard.sh` is a tracked wrapper that runs a gitignored hook if it
is present on disk and no-ops if it is absent — so a clean clone (which
has no local-only hooks) does not error on a missing script. It is the
plumbing for the local-only hooks below.

## `rules/` module

Shared rule modules imported by both the gate checks and the write-time
hooks, so write-time and check-time enforce the same patterns:

- `comments.py` — the 6-arm .rs comment detector (CJK, backtick,
  codename, doc-ref, own-crate name, comparison framing). Used by
  `check_rs_comments.py` and `hook_rust.py`.
- `naming.py` — test-fn name rules (prefix, segment cap, length, jargon).
- `paths.py` — `is_test_file` + the jscpd ignore globs.

## Misc

- `report_structure_facts.py` — emits the structure facts (file sizes,
  field counts, duplicate-code hotspots) consumed by the ratchet gates.
- `measure_app_coupling.py` — measures `App` coupling to scope the
  God-struct split; a report-only step, not a pass/fail gate.

## Local-only (gitignored)

A few hooks are gitignored local development aids. The gates invoke
them conditionally through `hook_guard.sh` — present they run, absent
they skip — so a clean clone is unaffected and they are not part of the
public build. `.gitignore` also carries patterns for additional
local-only gate scripts a contributor may add; anything matching those
patterns stays local.

The `.rs-comment-products` wordlist (gitignored) feeds the local-only
product-name scan if present.

## Adding a gate

1. Put the script under `scripts/` (python3, no third-party deps beyond
   what the repo already vendors).
2. Wire it into `check_code.sh` via `run_check` (blocking) or the
   report-only `|| echo` pattern.
3. If it ships a ratchet baseline, document the baseline inline so the
   ratchet can only ratchet down.
4. Add a `test_<name>.py` meta-test pinning both directions (must flag
   / must pass) so the gate itself is guarded against logic drift.
5. Keep it deterministic and fast — `make check` has a warm budget.
