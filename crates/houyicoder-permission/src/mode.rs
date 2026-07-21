//! Permission modes and the request snapshot the gate rules on. The gate
//! state, the validator registry wiring, and the per-mode policies live in
//! the gate module; this module holds only the value types shared across the
//! crate boundary (the mode enum, the request snapshot, the switch audit
//! record, and the parse error).

use serde_json::Value;

/// The agent permission modes. Two modes — Manual asks before any tool that
/// declares it needs approval (read-only tools still auto-allow); Auto allows
/// safe operations and only asks for destructive ones (the default). Shift+Tab
/// cycles Manual ↔ Auto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    /// Ask before tools that need approval; read-only auto-allows.
    Manual,
    /// Allow safe ops; ask destructive (until the recoverable invariant). The
    /// default.
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

impl std::fmt::Display for ModeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ModeError {}
