//! Shared request builders for the gate's test suites: the three tool shapes
//! (plain, bash, edit) the validators key on. Submodules pull these via
//! use super::* alongside the gate items re-exported through the glob.

use super::*;
use serde_json::Value;

pub(crate) fn req(name: &str, destructive: bool, read_only: bool, native: bool) -> ToolRequest<'_> {
    ToolRequest {
        tool_name: name,
        input: None,
        is_destructive: destructive,
        is_read_only: read_only,
        native_requires_approval: native,
    }
}

pub(crate) fn bash_req(command: &str) -> ToolRequest<'static> {
    // Borrow the command from a leaked JSON value so the request outlives
    // the helper. Tests are short-lived so the leak is bounded.
    let v: &'static Value = Box::leak(serde_json::json!({"command": command}).into());
    ToolRequest {
        tool_name: "bash",
        input: Some(v),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    }
}

#[cfg(test)]
mod egress;
#[cfg(test)]
mod fenced_exec;
#[cfg(test)]
mod git_ops;
#[cfg(test)]
mod invariants;
#[cfg(test)]
mod matrix;
#[cfg(test)]
mod mode_decision;
#[cfg(test)]
mod rule_dedup;
