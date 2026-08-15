#!/usr/bin/env bash
# Idempotent: install jscpd if absent. jscpd is the cross-file clone
# detector the L3 report (report_structure_facts) uses for the clone
# trend. Version-locked: jscpd's count shifts across versions, so an
# unlocked version makes the baseline meaningless -- the trend must be
# comparable across machines + CI. CI installs fresh; local devs run
# this once (or npm install -g jscpd@5.0.15).
set -e

JSCPD_VERSION=5.0.15

if command -v jscpd >/dev/null 2>&1; then
    exit 0
fi

if ! command -v npm >/dev/null 2>&1; then
    echo "jscpd absent and npm not found -- skipping (clone trend stays 'unavailable' in the L3 report)." >&2
    exit 0
fi

echo "installing jscpd@${JSCPD_VERSION} (clone detector for the L3 report)..."
npm install -g "jscpd@${JSCPD_VERSION}"
