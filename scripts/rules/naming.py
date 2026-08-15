"""Shared naming rules — single source for test-name limits (check_rust_naming
gate + hook_rust write-time hook).

Constants only: each caller applies them in its own scan context. Keeping
the limits here means the check-time gate and write-time hook agree without
a parallel wordlist to drift.
"""
import re

# Test-name limits shared by check_rust_naming (check-time gate) and
# hook_rust (write-time hook). <=4 underscores (5 words fragments the name);
# <=50 chars (trim, move detail into a doc).
TEST_UNDERSCORE_CAP = 4
TEST_NAME_LEN_HARD = 50

# Jargon blocklist for test fn + file names. "mid-run" is ad-hoc jargon (not a
# standard term); block so names use descriptive vocabulary (during_run,
# concurrent, control plane). "in-flight" is a standard distributed-systems
# term and is NOT blocked.
JARGON = re.compile(r"mid[-_]?run", re.IGNORECASE)
