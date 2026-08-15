#!/usr/bin/env bash
# Quick checks during development — static only, no tests.
set -euo pipefail
CARGO="${CARGO:-cargo}"
echo "▶ fmt-check";  "$CARGO" fmt --all -- --check
echo "▶ clippy";     "$CARGO" clippy --workspace --all-targets -- -D warnings
echo "✓ quick-check passed"
