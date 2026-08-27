#!/usr/bin/env python3
"""Layering dependency-graph assertion.

Reads every crate's Cargo.toml, extracts the houyicoder-* RUNTIME
dependencies (the [dependencies] section only; dev-dependencies are
excluded — tests may construct any real impl, so a dev-dep is test-only
coupling, not a layering direction violation), and checks each against
the layering whitelist. Any runtime dependency not in the whitelist is a
violation: a layer talking to a crate the architecture forbids.

The whitelist is the single source of truth for the dependency direction
(latest review of the layering design). Two read modes:

  default       report every violation; exit 1 if any. This is the binding
                gate wired into make check (via scripts/check_code.sh) and
                runs fail-fast: a runtime layering edge outside the
                whitelist fails the commit gate.
  --scope m1    report all, but exit 1 only on an edge M1 was supposed to
                clear. Historical milestone scope kept for the audit
                trail; the layering migration is complete, so default
                mode is the one that runs in the gate.

Run: make check-deps  (default)  |  python3 scripts/check_dep_graph.py --scope m1
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES = ROOT / "crates"

# The layering whitelist: which houyicoder-* crates each crate may depend on.
# Mirrors the layering design dependency rules. ports holds traits only;
# the engine (core) depends on ports + the foundation; L1 domains depend
# on ports + foundation; tui is a pure protocol client.
WHITELIST: dict[str, set[str]] = {
    "houyicoder-protocol": set(),
    "houyicoder-async": set(),
    "houyicoder-resilience": set(),
    "houyicoder-config": {"houyicoder-protocol"},
    "houyicoder-context": {"houyicoder-async"},
    "houyicoder-api": {
        "houyicoder-protocol",
        "houyicoder-context",
        "houyicoder-async",
    },
    "houyicoder-session": {
        "houyicoder-api",
        "houyicoder-protocol",
        "houyicoder-context",
        "houyicoder-async",
    },
    "houyicoder-memory": {
        "houyicoder-api",
        "houyicoder-protocol",
        "houyicoder-context",
        "houyicoder-async",
    },
    "houyicoder-graph": {"houyicoder-api", "houyicoder-protocol", "houyicoder-async"},
    "houyicoder-wasm": {"houyicoder-api", "houyicoder-protocol", "houyicoder-async"},
    "houyicoder-permission": {"houyicoder-api", "houyicoder-protocol", "houyicoder-async"},
    "houyicoder-skill": set(),  # S0: pure data processing (serde + serde_yaml + dunce + tracing + thiserror)
    "houyicoder-sandbox": {
        "houyicoder-api",
        "houyicoder-protocol",
        "houyicoder-context",
        "houyicoder-async",
        "houyicoder-resilience",
    },
    "houyicoder-provider": {
        "houyicoder-api",
        "houyicoder-protocol",
        "houyicoder-config",
        "houyicoder-async",
        "houyicoder-resilience",
    },
    "houyicoder-core": {
        "houyicoder-api",
        "houyicoder-protocol",
        "houyicoder-context",
        "houyicoder-async",
        "houyicoder-resilience",
    },
    # service is the composition root: may depend on core + every L1 + L0.
    "houyicoder-service": {
        "houyicoder-core",
        "houyicoder-api",
        "houyicoder-protocol",
        "houyicoder-context",
        "houyicoder-async",
        "houyicoder-resilience",
        "houyicoder-config",
        "houyicoder-session",
        "houyicoder-memory",
        "houyicoder-graph",
        "houyicoder-wasm",
        "houyicoder-permission",
        "houyicoder-sandbox",
        "houyicoder-provider",
        "houyicoder-skill",
    },
    "houyicoder-client": {"houyicoder-protocol", "houyicoder-async"},
    "houyicoder-tui": {"houyicoder-client", "houyicoder-protocol"},
    # cli is the binary entry point, above service in the stack. service owns
    # the composition logic (build_runner lives in service::composition); cli
    # is a thin binary that calls it and names the types the composition API
    # returns (Runner, SessionId, the mode gate). Those type references are the
    # legitimate surface a binary entry has over its composition root -- the
    # binary names what it wires -- so cli may reach core/context/permission
    # for type naming even though service owns the wiring.
    "houyicoder-cli": {
        "houyicoder-tui",
        "houyicoder-client",
        "houyicoder-service",
        "houyicoder-protocol",
        "houyicoder-config",
        "houyicoder-context",
        "houyicoder-permission",
        "houyicoder-core",
        "houyicoder-api",
    },
    # The loader is a leaf: it only reads the context crate's event types +
    # writes a session dir. No runtime, no TUI, no server -- a standalone
    # ecosystem-compat tool.
    "houyicoder-loader": {"houyicoder-context"},
}

# Edges the M1 milestone was scoped to clear. Kept for the --scope m1 audit
# trail; all listed edges are now dev-dependencies (test-only, excluded from
# the runtime check), so default mode no longer sees them as violations.
M1_CLEARED_EDGES = {
    ("houyicoder-provider", "houyicoder-core"),
    ("houyicoder-permission", "houyicoder-core"),
    ("houyicoder-core", "houyicoder-session"),
    ("houyicoder-core", "houyicoder-sandbox"),
    ("houyicoder-core", "houyicoder-memory"),
    ("houyicoder-session", "houyicoder-memory"),
}

DEP_RE = re.compile(r"^(houyicoder-[a-z]+)\s*=", re.MULTILINE)

# External crates that must not leak past a single boundary crate. The
# upstream agent-client-protocol crate is the schema the protocol layer
# mirrors; only service may depend on it (optional, gated by the
# acp-cross-decode feature for the cross-decode fidelity tests), so typed
# ACP shapes never cross the service boundary into client/tui/protocol/core.
# A new external boundary joins by being added here. Checked across runtime
# + dev sections: a dev-dep on a gated external is still test-only coupling
# that must not leak past the boundary crate.
EXTERNAL_BOUNDARY: dict[str, set[str]] = {
    "agent-client-protocol": {"houyicoder-service"},
}


def collect_external_violations() -> list[tuple[str, str]]:
    """Return (crate, external_dep) pairs where a crate outside the boundary
    set declares the gated external crate."""
    violations: list[tuple[str, str]] = []
    for cargo in sorted(CRATES.glob("houyicoder-*/Cargo.toml")):
        crate = cargo.parent.name
        text = cargo.read_text(encoding="utf-8")
        for ext, allowed in EXTERNAL_BOUNDARY.items():
            if crate in allowed:
                continue
            if re.search(rf"^{re.escape(ext)}\s*=", text, re.MULTILINE):
                violations.append((crate, ext))
    return violations


def crate_deps(cargo_toml: Path) -> set[str]:
    """Return the houyicoder-* RUNTIME dependencies declared in
    [dependencies]. Dev-dependencies are excluded: tests may construct any
    real impl, so a dev-dep is test-only coupling, not a layering direction
    violation. The layering whitelist governs runtime deps."""
    text = cargo_toml.read_text(encoding="utf-8")
    deps: set[str] = set()
    section = None
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped[1:-1].strip()
            continue
        # Only the runtime dependency section; skip dev-dependencies (test-only).
        if section != "dependencies":
            continue
        m = DEP_RE.match(line)
        if m:
            deps.add(m.group(1))
    return deps


def collect_violations() -> list[tuple[str, str]]:
    """Return (crate, dep) pairs where crate depends on a dep not in its
    whitelist. Crates without a Cargo.toml or not in the whitelist are
    skipped (new crates join by being added to WHITELIST)."""
    violations: list[tuple[str, str]] = []
    for cargo in sorted(CRATES.glob("houyicoder-*/Cargo.toml")):
        crate = cargo.parent.name
        if crate not in WHITELIST:
            continue
        allowed = WHITELIST[crate]
        for dep in crate_deps(cargo):
            if dep == crate:
                continue  # self-reference
            if dep not in allowed:
                violations.append((crate, dep))
    return violations


def main() -> int:
    scope = ""
    if len(sys.argv) > 2 and sys.argv[1] == "--scope":
        scope = sys.argv[2]

    ext_violations = collect_external_violations()
    if ext_violations:
        print("dep-graph: external-crate boundary violations (blocking)")
        for crate, ext in ext_violations:
            allowed = ", ".join(sorted(EXTERNAL_BOUNDARY[ext]))
            print(f"  {crate} -> {ext} (only [{allowed}] may depend on it)")
        print(f"\n{len(ext_violations)} external boundary violation(s)")
        return 1

    violations = collect_violations()
    if not violations:
        print("dep-graph: clean — every crate within its whitelist")
        return 0

    print("dep-graph: violations (crate -> dep not in whitelist)")
    for crate, dep in violations:
        print(f"  {crate} -> {dep}")
    print(f"\n{len(violations)} violation(s) total")

    if scope == "m1":
        m1_remaining = [v for v in violations if v in M1_CLEARED_EDGES]
        if m1_remaining:
            print("\nM1-scope edges still violating:")
            for crate, dep in m1_remaining:
                print(f"  {crate} -> {dep}")
            print(f"{len(m1_remaining)} M1 edge(s) not yet cleared")
            return 1
        print("\nM1-scope edges all cleared (other violations are deferred)")
        return 0

    return 1


if __name__ == "__main__":
    sys.exit(main())
