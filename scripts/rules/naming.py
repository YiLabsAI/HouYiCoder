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

# Top-level module whitelist for the four core crates (002 §6.3–§6.4).
# Shared by check_rust_naming (check-time) and hook_rust (write-time).
ALLOWED_TOP_MODULES: dict[str, frozenset[str]] = {
    "crates/houyicoder-tui/src": frozenset({
        "lib", "app", "command", "composition", "input", "keys", "session",
        "transcript", "ui", "view", "workflow", "bench_api", "frame_timing",
        "test_harness",
    }),
    "crates/houyicoder-core/src": frozenset({
        "lib", "agent", "observability", "provider", "snapshot",
    }),
    "crates/houyicoder-core/src/agent": frozenset({
        "append", "builder", "compaction", "context", "exports", "hook",
        "inference", "memory", "multi_agent", "runner_config", "skill",
        "status", "tests", "tools", "turn", "worktree",
    }),
    "crates/houyicoder-service/src": frozenset({
        "lib", "acp", "agent_registry", "composition", "diagnostics",
        "lifecycle", "protocol_adapter", "sandbox_policy", "server", "uds",
    }),
    "crates/houyicoder-cli/src": frozenset({
        "main", "cli_args", "composition", "detach", "export_view",
        "local_host", "session", "tests", "trajectory",
    }),
}

# Exact legacy production mod.rs paths under crates/*/src/ (pinned by path,
# not count, so deleting one cannot license creating another).
ALLOWED_MOD_RS: frozenset[str] = frozenset({
    "crates/houyicoder-core/src/observability/mod.rs",
    "crates/houyicoder-protocol/src/acp_wire/mod.rs",
    "crates/houyicoder-protocol/src/frontend/mod.rs",
    "crates/houyicoder-skill/src/disclose/mod.rs",
})
