# houyicoder Development Guidelines

This document is the single source of truth for engineering rules, mirroring
established agent engineering guidelines and translated to Rust + houyicoder's architecture.
CONTRIBUTING.md is intentionally minimal and points here.

## Architectural Principles

Non-negotiable. Violating them requires explicit approval + a migration plan.

### 1. Plan First
Industrial-grade design: plan before implementing. Design docs required for
features touching multiple crates. Non-functional targets:

| Metric | Target |
|--------|--------|
| Build (cargo check workspace) | < 30s incremental |
| Test gate (make check) | < 30s warm, < 120s cold (CHECK_BUDGET_WARM/COLD) |
| Clippy | Zero warnings (-D warnings) |
| rustfmt | Zero diffs |

### 2. Type First (Schema First, Rust)
All public boundaries must be explicitly typed.
- No Box<dyn Any> / untyped boundaries; use concrete types or trait objects
  with typed interfaces.
- Protocol types derive serde::Serialize/Deserialize; versioned + backward-
  compatible.
- Capability tokens are typed enums, not strings.

### 3. Data-Driven & Self-Evolving
Metrics drive decisions; new capabilities require measurable eval criteria;
runtime metrics inform optimization, measured by the eval harness.

### 4. Immutable State
Prefer append-only event logs over in-place mutation. State transitions
produce new versions. Critical for checkpoint/restore and replay
(SessionStore).

### 5. Full-Stack Observability
Every critical path traceable end-to-end. OpenTelemetry spans; structured
logs with correlation IDs; metrics exported. **No silent failures** — tool
errors surface with context.

### 6. Security by Default
- unsafe_code = "deny" (workspace lint, enforced in make check).
- No panics in library code — return Result; expect/unwrap only in
  tests or behind a documented invariant.
- Untrusted data (memory recall, file contents, tool/MCP results, graph
  summaries) enters the prompt under an explicit untrusted-data framing.
- Capability-scoped, deny-by-default for all guests.

### 7. Documentation as Code
- /// doc comments required on all public items. Write prose with bare
  identifiers (no backtick markup) -- same rule as // comments, enforced by
  scripts/check_rs_comments.py. Rustdoc renders Markdown structure (lists,
  emphasis) fine, but code identifiers go bare, not in backticks.
- Doctests (/// # Examples) for non-trivial public APIs.
- Crate-level //! docs explain the crate's role + links to design docs.

### 8. Architecture Artifacts
Major features need a design doc with Mermaid diagrams. Required: component
diagram for new crates; sequence diagram for cross-crate flows. The master
architecture diagram MUST be updated when layering changes.

### 9. Dependency Governance
- Minimal dependencies; prefer std over external crates.
- All new deps require review: why (no std/alternative?), license (MIT/
  Apache preferred), maintenance, transitive impact.
- cargo deny check (licenses + RUSTSEC advisories) once wired.
- MSRV declared in rust-toolchain.toml.

### 10. Bug-Driven Testing
Bug fixes follow red-green-refactor: write a failing test that reproduces
the bug, then fix. Tests enable safe refactoring.

### houyicoder-specific architectural rules (inviolable)
- **One host, many guests.** The Rust daemon holds all trust; every other
  component is a guest via the protocol. Guests never share the host heap.
  No in-process bypass of the protocol (a common pitfall in agent CLIs).
- **No scripting runtime in the host.** Workflows are declarative data;
  TypeScript only compiles to WASM for plugins.
- **Control plane ≠ model plane.** Determinism lives in the control plane;
  the model plane executes one bounded step. Non-deterministic leaves (LLM
  responses, memory recall, graph queries) are snapshot-persisted for replay.
- **Reproducibility = control-plane-flow reproducible + snapshot-replay**,
  not literal re-run.
- **Token is a first-class budget** — explicit, measured, cache-aware.

## Repository Layout

- crates/ — the workspace: houyicoder-{api, async, cli, client, config,
  context, core, graph, loader, memory, permission, protocol, provider,
  resilience, sandbox, service, session, tui, wasm}.
- scripts/ — the gate: check_code.sh (full), quick_check.sh (fmt + clippy),
  plus the per-rule checkers each gate step runs.
- .github/ — CI.

**Policy**: respect crate boundaries; cross-crate changes split into small,
reviewable commits with regression coverage.

## Development Environment

| Tool | Purpose |
|------|---------|
| Rust | stable (pinned via rust-toolchain.toml) |
| Components | rustfmt, clippy, rust-src |
| Format/lint | rustfmt + clippy (workspace lints in Cargo.toml) |
| Type check | cargo check (Rust's type system) |
| Tests | cargo test (or cargo-nextest for speed) |
| Coverage | cargo-llvm-cov (optional) |
| Dep audit | cargo-deny (optional) |

```bash
make install   # stable + rustfmt + clippy
```

## Development Workflow

```bash
make install       # one-time toolchain setup
make quick-check   # fmt-check + clippy (fast, during dev)
make check         # full pre-commit gate (fmt + clippy + typecheck + test)
make check-full    # check + coverage gate (what the pre-push hook runs)
make verify        # check-full + the ignored suite (sandbox, PTY UI) + doc-stale
make format        # cargo fmt --all
make lint          # clippy -D warnings
make test          # cargo test --workspace
make clean
```

**Always run make check before commit.** Two gate tiers: `make check` is the
commit gate (fast unit suite + diff coverage); `make verify` is the done gate
(adds the ignored cross-layer, PTY, and sandbox suites). A change that touches
cross-layer or interactive behavior is not finished until verify is green —
`make check` alone cannot see those paths.

## Coding Standards

### Formatting & Lint
- cargo fmt --all is authoritative; fmt --check is a gate.
- clippy -- -D warnings is a gate (workspace lints: clippy::all warn,
  unsafe_code deny; too_many_lines / cognitive_complexity warn → deny
  in the gate). Pedantic is NOT enabled as a group — enable individual
  pedantic lints on demand.
- **Process spawn is a chokepoint.** clippy.toml bans
  `std::process::Command::new` and `tokio::process::Command::new` via
  disallowed-methods. Every spawn routes through the ProcessLauncher port so
  the resource fence, wrapper, and audit policy apply uniformly. Pre-existing
  direct spawns are allow-flagged at their call sites as the migration list; a
  NEW direct spawn fails the gate. Test scaffolding that is not an engine
  spawn may allow-flag with a comment saying why.

### Other gates you will hit
Each is a separate step in make check with its own error message; these are
the ones whose fix is not obvious from the message alone.

| Gate | Rule |
|------|------|
| dep-graph | Crate runtime dependencies must match the layering whitelist. A new cross-crate `[dependencies]` edge fails unless the architecture allows that direction. dev-dependencies are exempt (test-only coupling is not a layering violation). |
| comment-block-length | A consecutive in-function `//` block over 12 lines warns; the count is baselined and pinned. Long rationale belongs in a doc comment on the item, not a wall inside the body. |
| dead-code-ratchet | The number of whole-file `#![allow(dead_code)]` suppressions is strict-pinned to a baseline. Both growth and drift fail: narrow the suppression to the specific item, or delete the dead code, rather than blanketing a file. |

### Public surface is declared, not inherited
- No glob re-export (`pub use foo::*`) at a crate root. A glob makes any
  pub item in a submodule crate-public the moment it is written, with no
  line in the diff to review as an API decision, and leaves two public
  paths per type. Re-export by name.
- Prefer a crate root that holds no types: module declarations plus the
  named public surface. A root that carries content is both a file that
  hits the size gate and an API surface, and every edit then negotiates
  with the gate first.
- Modules split on file-size grounds stay private behind the named
  re-exports. Making them pub freezes an incidental layout into public
  contract, so a later re-split becomes a breaking change.
- Inside the crate, import from the defining module (crate::ids::X), not
  through the crate's own facade (crate::X). Routing internal deps through
  the public surface hides the real dependency graph.

### Read-modify-write belongs behind one call
- A store trait that exposes read and write separately invites a lost
  update: two callers each write a whole record derived from the state
  they read, and the later write silently reverts the earlier one's
  field. Expose the composed operation (update taking an edit closure)
  and serialize the read and the write inside the impl.
- Make it a required trait method, not a defaulted one. A default that
  composes read and write is non-atomic, so every future impl inherits
  the bug instead of being made to decide.
- A shared temp-file name in an atomic write is the same class, one level
  down: two writers truncate the same tmp, so one can rename a file
  holding the other's half-written bytes. Name it per writer (pid plus a
  counter).

### Code comment style
.rs comments must be plain English prose. Forbidden in code comments:
Chinese characters; backtick-quoted identifiers (write the bare identifier,
no markup); short letter-and-digit labels standing in for a concept, and
numbered stage or phase names — spell out what the thing does instead;
names of other products or tools — comments describe this codebase's own
design, not how it compares to others. Enforced by
scripts/check_rs_comments.py in make check.

**A comment must stand on its own.** State the reasoning; do not cite where
it is written down. A pointer to something outside the code is one the reader
may be unable to follow, and it rots on its own schedule while the code moves
on. So no document paths or filenames, no "see the design doc", and no
milestone, iteration, or lettered requirement id. If a decision matters to
someone editing the code, the comment carries the decision; if it does not,
it does not belong in a comment at all. This bans the citation, not the
substance — writing out why a rule exists is exactly right. Runtime
artifacts the product itself reads (agent.md, MEMORY.md, AGENTS.md) are
domain objects, not citations, and stay allowed. Same enforcement.

### Bulk edit protocol
When editing many files via a script (sed or python regex across the tree):
back up first (git stash or cp -r) so it is reversible; dry-run on one file
and review the diff before applying broadly; verify after (grep, cargo
check, make check). Prefer targeted edits over blanket regex. Blanket
regex without a dry-run is a known cause of avoidable breakage (mangled
indent, leftover residue, hit string literals).

### Code size & complexity (refactor triggers)
- **Function**: clippy::too_many_lines fires >100 lines → refactor/split.
- **Cognitive complexity**: clippy::cognitive_complexity > 30
  (clippy.toml) → refactor.
- **File**: scripts/check_file_size.py, per .rs file. Production warns ≥500,
  **errors ≥800**; test files warn ≥800, **error ≥2000** (a table-driven test
  file is legitimately longer than the code it covers). 500–800 is the
  refactor band for production: a file in that range is a prompt to refactor
  on SRP grounds at the right time, not a hard chase to stay under 500.
- Hitting a threshold is a mandate to refactor, not a warning to ignore.
  World-class code, no garbage.

### Doc comments
- /// on all public items; Markdown is fine (rustdoc).
- Crate-level //! explains role + design-doc links.

### Error handling
- Validate early at boundaries.
- Return Result; structured error enums per crate (thiserror-style when
  a dep is added).
- No unwrap/expect in non-test library code except behind a documented
  invariant.
- Tool execution failures surface with context, never silent.

### Where a message goes (three sinks)
The TUI runs in the terminal's alternate screen, which does not capture
stdout or stderr. A print macro from library code is painted by the
terminal wherever the cursor sits — during a session that is **inside the
input box**. So "I do not want to swallow this error, and I do not want to
propagate it, so eprintln" corrupts the surface the user is typing into.
Choosing the sink is a design decision, not a matter of taste:

| Sink | When | How |
|------|------|-----|
| User-visible | The user must know, or can act on it | System line — `LiveEvent::SystemLine` through the runner's live sink; lands in the transcript, survives scrollback |
| Diagnostic | Only a developer can use it | `tracing` macros (`tracing::warn!`, `tracing::debug!`); a file-backed subscriber installed at the composition root, toggled at runtime via the `/debug` wire command. Never the terminal |
| Console | No alternate screen is up: argument parsing, startup failure, a non-TUI binary | A print macro, correct here and only here |

- Enforced by scripts/check_stderr.py. Two tables: `_CONSOLE_OK` (correct
  console writes, permanent) and `_STDERR_BASELINE` (pre-existing runtime
  writes awaiting migration, ratchets to empty). Both are matched exactly
  per file, so a count that drifts in either direction fails. The baseline
  is currently empty — all runtime writes have migrated to tracing.
- Counted per file rather than exempting a file, because both classes share
  files: the cli entry point holds argument parsing and post-alternate-screen
  wiring in one module, and a whole-file exemption would bless exactly the
  call sites that need guarding.
- Test and example targets are exempt by structure (tests/, examples/,
  benches/, *_tests.rs, and the trailing `#[cfg(test)] mod`) — there their
  output IS the product.
- **A best-effort failure is not exempt from having a sink.** Silently
  dropping it violates the no-silent-failure rule; eprintln-ing it is not a
  channel, just an unrouted write. Pick user-visible or diagnostic.

### API stability
- No breaking signature changes without migration notes.
- New params: prefer #[non_exhaustive] + builder.

## Testing

| Category | Description | Tool |
|----------|-------------|------|
| Unit | Fast, deterministic, single module; mock collaborators at boundaries. | cargo test (lib) |
| Integration | Cross-crate real collaboration; local, deterministic. | cargo test (integration tests) |
| Spike | Verification of a load-bearing tech bet (not a feature test). | standalone bins |
| Live | Real providers/network — opt-in, NEVER in default gates. | env-gated |

### Layout policy
- Tests live under each crate's tests/ or inline #[cfg(test)] modules.
- One source module → one peer test file (foo.rs → tests/foo.rs or
  inline mod foo_tests).
- Tests with no single source peer (cross-module behavior paths, e.g. a
  gate's decision logic spanning many validators) use a `tests/` directory
  module under the host source file (`gate.rs` + `gate/tests/{domain}.rs`),
  submodules named by behavior domain — not `X_tests.rs` / `X_extra_tests.rs`,
  which imply a source peer that doesn't exist or an overflow split.
- Test directories mirror source structure; no "deep source + shallow test".
- Only integration/ and live/ may break source mirroring.
- Submodules use Rust's first-class directory modules, not a filename-prefix
  simulation: a directory holding both X.rs and X_sub.rs is a flat prefix
  faking a hierarchy. Split to X.rs + X/sub.rs so the path is X::sub (no
  stutter -- X_sub::item repeats the X concept twice) and the file tree
  shows the subsystem. The 2018 form (X.rs + X/) is preferred over mod.rs.
  Enforced by check_rust_naming.py rule 8; the baseline of pre-existing
  flat prefixes ratchets down as each pair converts. _tests.rs peers and
  tests/ integration binaries are exempt (foo.rs + foo_tests.rs is the
  one-source-one-peer-test convention, and each tests/*.rs is its own
  binary, not a module).

### Mock the bottom (unit tests)
- Unit tests NEVER hit real external calls: no real LLM/provider HTTP, no
  sandbox-exec spawn, no real network, no heavy lazy init (e.g. the
  ~300ms tiktoken BPE table). A unit test that pays a real-subsystem cost
  is an integration test mislabeled as unit -- move it to tests/.
- A test that drives REAL cross-layer collaboration — a real Runner + Server
  + wire frame I/O + the serve loop, even with the provider mocked — is an
  INTEGRATION test, not a unit test. "No real external call" is the floor;
  no real INTERNAL cross-layer collaboration is the rule. Drive the unit
  under test with mocked collaborators; if the test spins up a real Runner
  or Server or pumps wire frames through a real serve loop, it belongs in
  tests/. Inline `src/*_tests.rs` is for tests that can access private
  items of ONE unit with all collaborators mocked.
- Classification gate before writing: ask "does this test build a real
  Runner/Server or drive a real serve loop / wire dispatch?" If yes →
  tests/ (integration, public API only). If no (pure logic + mocked
  collaborators) → src/ inline. Do not default to src/ inline because it
  is convenient (private access + runs in the gate); the directory follows
  the responsibility, not the convenience.
- Mock at the boundary the harness owns: StubProvider / ScriptProvider for
  the LLM (canned replies), StubTool for tools (no subprocess), a fast
  tokenizer path (HOUYICODER_FAST_TOKENS) so token counting does not load
  the BPE table. The two tokenizer accuracy tests opt out via real().
- A new external dependency in a unit-tested path must come with a mock,
  not a real call. run_tests.py sets HOUYICODER_FAST_TOKENS for the unit
  suite; production never sets it.
- Target: every unit test under ~20ms. A unit test over 20ms is a smell
  (a real call slipped in, or a heavy init is paid per test).

### Assert the visible, not the internal (behavior tests)
- A test for a render/locate/jump/highlight behavior must assert what the user
  SEES (rendered text or cell style), not an internal field (a focus index, a
  scroll offset, an `open` flag). Internal fields are the MEANS; the visible
  output is the END. A means-assertion stays green when the means is correct
  but the end is broken — `focus == newest_match` passes while the screen
  never scrolls to that match.
- Render the app to a test backend and assert on the buffer (text or cell
  styles). The cost is the same as asserting an internal field; the coverage
  is strictly higher.
- A green test has two indistinguishable causes: the code is correct, or the
  test does not exercise the code path. For behavior tests, asserting the
  rendered output is what separates them.

### Mutation-verify cross-path invariant tests
- A test that asserts "two independent paths must agree" (count == render,
  index == render, a truncated form == its full form) is silent when the
  invariant holds for the wrong reason: the corpus never triggers the branch
  where the two paths could diverge, so both return the same value trivially
  and the test passes without testing.
- Before committing such a test, mutation-verify it: revert each coordinated
  site in the implementation ONE AT A TIME, run the test, confirm it reddens.
  Reverting all sites at once only proves "at least one is guarded"; reverting
  each proves each is. Restore after.
- Scope this narrowly: ONLY cross-path invariant tests. A pure
  `fn(input) -> output` test (input maps directly to output, no second path)
  has no silent-agreement surface and does not need this — do not blanket the
  whole suite; the mutate-run-restore cost is wasted where it cannot silently
  pass.

### Flaky tests: widen the window, do not count green runs
- N green runs is not evidence for a test that fails 1 in N. Twenty passes
  against a 1-in-20 flake is a coin flip, and it is the wrong environment
  anyway: the gate runs the ignored suite 8-way parallel, which is where
  the contention that reddens it lives.
- Diagnose by widening the window, then mutate both ways: inject a delay
  at the suspected point, confirm the OLD form reddens deterministically
  and the NEW form passes. That turns a probability into a proof, and it
  tells you which window you actually fixed. Remove the probe after.
- A one-shot filesystem check after a UI sync point is the recurring
  shape. The render does not wait for the durable write, so at the moment
  the text is on screen the directory may not exist, and an atomic publish
  (write tmp, fsync, rename) may be mid-flight. Poll the whole discovery
  including the directory, not just the leaf file; polling one level down
  leaves the same bug one level up.

### Shared test helpers (no duplication)
- Helpers shared across more than one tests/ binary (stub runner, channel
  pair, frame send/recv) live in `crates/<crate>/tests/common/mod.rs`.
  Each binary does `mod common;` + imports them. Cargo treats
  `tests/common/mod.rs` as a helper module (not a separate test binary).
- Do not copy-paste a helper into a second binary; promote it to common
  when the second caller appears.

### Naming
- Test fn pattern: `test_<subject>_<behavior>[_<condition>]`. Name what the
  code DOES (behavior), not how it is built (implementation).
- Segment budget: prefer 2-3 segments after `test_` (full name 2-3
  underscores). More than 4 underscores (full name) errors the gate; a name
  at the 4-underscore cap should usually be trimmed further. Push scenario
  detail into a module doc, not the name.
- Behavioral, not implementation-detail: `test_tail_summarizes_older` (what
  it does) over `test_tail_verbatim_older` (how it is stored). Words like
  "verbatim" name an implementation concept; tests name behavior.
- No module-context repetition: `test_write_file_creates_parents`, not
  `test_write_file_executor_creates_parents` (the file path already says
  executor).
- Plain words only, no uncommon/jargon terms (keeps over verbatim; rows
  over chip). If a word needs explaining it is too uncommon for a test name.
- World-class naming: align with established open-source vocabulary. Use
  program-design + distributed-systems terms, not ad-hoc jargon.
  Prefer: control plane / data plane (for command vs execution paths),
  concurrent / interleaved (for "while something runs"), active / pending /
  in-flight (for an ongoing operation — in-flight requests is a standard
  networking/LB term, same tier as active/pending), boundary / collaborator
  (for "the other layer"), transport / framing (for wire I/O). Avoid:
  "mid-run" (ad-hoc; use during_run / concurrent), "wire-it", "the X
  thing". Name the CONCERN in domain terms a reader from any codebase
  recognizes.
- No project-specific prefixes or codenames (e.g. `bug1_`, internal ids);
  describe the behavior.
- Do not encode execution tier in names (no test_smoke_* / test_full_*);
  use #[ignore] / CI selection.
- Length: 40 chars soft warn, 50 hard error (the gate's limits); aim well
  under the warn rather than at the cap.
- Good: `test_write_file_creates_parents`, `test_tail_summarizes_older`,
  `drag_copies_agent_line`. Bad: `test_write_file_executor_creates_parents`,
  `test_vertex_gemini_build_generate_config_ignores_parallel_tool_calls`.
- Enforced by scripts/check_rust_naming.py (detects test fns by #[test] /
  #[tokio::test] attribute, requires the `test_` prefix, supports async fn,
  caps underscores + length, and rejects flat-prefix module pairs) in make
  check. Convention follows an established Test Function Naming standard.
- Types UpperCamelCase, fns snake_case, consts SCREAMING_SNAKE_CASE,
  modules snake_case -- enforced by rustc lints under -D warnings.

### Coverage
- Gate: workspace unit line coverage ≥ 85% (check_coverage.sh, COV_THRESHOLD)
  and ≥ 85% of the lines a branch adds or modifies (check_diff_coverage.py,
  COV_DIFF_THRESHOLD). Both raise to 90 once comfortably green.
- Risk-driven over a fixed count: cover happy path + boundary + error path +
  one dependency-interaction assertion per non-trivial module.
- diff-cov (make check) reads the --lib lcov only; integration tests in
  tests/ do NOT contribute. Moving a test from inline (src/) to tests/
  moves its coverage of production lines OUT of the lib lcov. That is
  correct ratchet behavior: make check passes while no production line
  changed vs HEAD (no diff to gate); the next change to a line whose
  coverage moved to integration is required to bring that line to the diff
  threshold in lib coverage (add a lib test or accept the gate). Do NOT
  revert a correct src/→tests/ move because a future change "would fail
  diff-cov" — that future failure is the gate doing its job (ratcheting
  coverage up on the next edit). Before moving, confirm the production
  lines still have lib coverage if you want them gated at pre-commit;
  otherwise the move is fine + the ratchet handles the rest.

## Collaboration

### Windows verification

CI gates Windows, but a round trip per defect is the wrong loop. Verify
locally: `brew install llvm` (for clang-cl) + `cargo install cargo-xwin`,
then `cargo xwin check --target x86_64-pc-windows-msvc --workspace
--all-targets` (and `cargo xwin test` for runtime defects). xwin fetches the
Microsoft CRT and Windows SDK, the sysroot the TLS stack needs to build from
a non-Windows host. Use a separate `CARGO_TARGET_DIR` so it does not clobber
the native build cache.

Four patterns fail on Windows and cannot be seen on macOS, where
`cfg(unix)` is always true and paths have no backslashes:

- **`format!` building JSON around a path.** A backslash is an invalid JSON
  escape, so the value parses as nothing and the field silently disappears.
  Build with `serde_json::json!` and let the serializer escape.
- **A read-only handle passed to `set_times`.** Windows needs
  `FILE_WRITE_ATTRIBUTES`. Use `OpenOptions::new().write(true)` for files —
  correct on both platforms. Directories need opposite opens (read on Unix,
  since write gives EISDIR; write plus `FILE_FLAG_BACKUP_SEMANTICS` on
  Windows), so a helper that stamps directories must split on `cfg`.
- **`#[cfg(unix)]` on test functions but not their shared helpers**, which
  leaves the helpers dead on Windows. Gate the module instead:
  `#[cfg(all(test, unix))] mod tests`.
- **An unconditional `use` of a type only referenced under `#[cfg(unix)]`**,
  an unused import on Windows. Gate the import to match.

Reference implementations are platform-correct, not portable: copying a
crate's `cfg(windows)` body into a shared helper breaks Unix. Read the cfg
boundary along with the code.

### PR checklist
- [ ] Types updated; serde versions where protocol-facing
- [ ] No breaking public API changes (or migration notes)
- [ ] make check passes
- [ ] New critical paths include OTel spans / structured logs
- [ ] Tool execution respects capability/sandbox boundaries
- [ ] Untrusted data framed in prompts (if touching ContextPlanner)
- [ ] Design doc + Mermaid diagram updated (if layering changed)

## Version Management

Semantic Versioning 2.0.0. Before release: make check green, CHANGELOG
updated, rust-toolchain.toml MSRV current.

## Commit standards (mandatory)

- Atomic commits: one logical change per commit.
- Conventional commits: first line MUST match type(scope?): subject
  - Types: feat / fix / docs / refactor / test / chore / style / perf / ci / build
  - Optional scope: feat(tui): ...
  - Subject <= 72 chars, imperative mood (add not added)
  - Body (optional): wrap at 72, explain why not what
- Commit messages are written for external readers, in English: imperative
  subject under 72 chars with a conventional prefix; an optional body of one
  or two sentences explaining why (the diff already says what); factual and
  neutral in tone; no internal process references.
- Linear history: git pull --rebase; squash local WIP before push.
- Enforced by scripts/commit_msg_check.sh (structural: prefix + length) plus
  an optional local style lint the script reads if a gitignored wordlist is
  present. Install via make setup-hooks. The pre-push hook re-scans the
  about-to-be-published range -- published history is immutable, so pre-push
  is the final gate.
- If you add/rename a major abstraction, update the architecture docs and
  this guide.
