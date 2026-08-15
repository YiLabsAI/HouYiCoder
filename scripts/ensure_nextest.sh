#!/usr/bin/env bash
# Idempotent: install cargo-nextest if it is absent. nextest runs test
# binaries in bounded parallel (cargo test runs them serial), so the full
# suite finishes faster and a single test crash does not abort the rest.
# Portable: works on any machine with the Rust toolchain (cargo + a
# compiler). CI caches ~/.cargo so the install is one-time. Prefer
# cargo-binstall when present (prebuilt binary, seconds); fall back to
# cargo install (compile from source) so the script works everywhere.
set -e

if cargo nextest --version >/dev/null 2>&1; then
    exit 0
fi

if cargo binstall --version >/dev/null 2>&1; then
    echo "installing cargo-nextest via cargo-binstall (prebuilt)..."
    cargo binstall --no-confirm --locked cargo-nextest
else
    echo "installing cargo-nextest via cargo install (compile; one-time, CI caches)..."
    cargo install --locked cargo-nextest
fi
