# Contributing to houyicoder

Thank you for your interest. This document is intentionally minimal — the
single source of truth for engineering rules is [AGENTS.md](AGENTS.md).

## Quick Start

```bash
git clone git@github.com:YiLabsAI/HouYiCoder.git
cd HouYiCoder
make install        # stable Rust + rustfmt + clippy
```

## Contribution Loop

1. Create a branch.
2. Implement changes + tests.
3. Run local checks: make check.
4. Open a PR against main.

## Quality Gates (required)

Follow [AGENTS.md](AGENTS.md) exactly. In particular:
- cargo fmt --all -- --check clean.
- cargo clippy --workspace --all-targets -- -D warnings clean.
- cargo test --workspace green.

## Useful Commands

```bash
make help          # all commands
make quick-check   # fmt-check + clippy (fast)
make check         # full pre-commit gate
make format        # cargo fmt
make lint          # clippy
make test          # cargo test
make benchmark     # verification spikes
make clean
```
