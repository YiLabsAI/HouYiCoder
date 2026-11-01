//! Permission modes and the request snapshot the gate rules on. The gate
//! state, the validator registry wiring, and the per-mode policies live in
//! the gate module; this module holds only the value types shared across the
//! crate boundary (the mode enum, the request snapshot, the switch audit
//! record, and the parse error).

use serde_json::Value;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;

/// The agent permission modes. Two modes — Manual asks before any call it
/// cannot prove read-only and non-destructive; Auto allows operations the
/// side-effect table classifies as exec or filesystem (the pipeline has
/// already caught dangerous commands) and asks before network, before a call
/// the tool declares it must approve, and before a destructive call whose
/// name the table leaves unclassified (the default). Shift+Tab cycles Manual
/// and Auto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    /// Ask unless the call is read-only and non-destructive.
    Manual,
    /// Allow classified exec and filesystem; ask network, native approval,
    /// and an unclassified destructive call. The default.
    Auto,
}

impl PermissionMode {
    /// Whether Shift+Tab may cycle into this mode. Both modes cycle.
    pub fn is_tab_cycleable(self) -> bool {
        true
    }

    /// Whether entering this mode needs an explicit /mode plus a confirm. Both
    /// modes cycle with one key (no confirm); Auto is safe today because it
    /// still asks for destructive ops.
    pub fn requires_confirm(self) -> bool {
        false
    }

    /// The sandbox stays on in every mode. Auto skips approval only for safe
    /// ops; it never disables the fence. Always true; consulted by GuardedTool
    /// so a future relax call cannot weaken it.
    pub fn sandbox_mandatory(self) -> bool {
        true
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Result<Self, ModeError> {
        match s.to_ascii_lowercase().as_str() {
            "manual" => Ok(Self::Manual),
            "auto" => Ok(Self::Auto),
            other => Err(ModeError(format!("unknown mode: {other}"))),
        }
    }

    /// The next mode when Shift+Tab cycles: Manual ↔ Auto (2-state).
    pub fn tab_next(self) -> Option<Self> {
        match self {
            Self::Manual => Some(Self::Auto),
            Self::Auto => Some(Self::Manual),
        }
    }
}

/// A snapshot of a tool call the gate can rule on. The input is optional
/// because the runner calls requires_approval with no input; execute
/// re-checks with the real input so content, safety, and compound rules fire
/// at the enforcement point.
#[derive(Debug, Clone)]
pub struct ToolRequest<'a> {
    pub tool_name: &'a str,
    pub input: Option<&'a Value>,
    pub is_destructive: bool,
    pub is_read_only: bool,
    pub native_requires_approval: bool,
}

/// A mode switch audit record. /mode-log replays these.
#[derive(Debug, Clone)]
pub struct ModeChange {
    pub from: PermissionMode,
    pub to: PermissionMode,
    pub reason: String,
}

#[derive(Debug)]
pub struct ModeError(pub String);

impl Display for ModeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for ModeError {}
