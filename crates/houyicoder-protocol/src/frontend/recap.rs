//! Payloads of the recap surface: the focus a terminal reports and the
//! summary text the host sends back.
//!
//! Both cross between the host and a frontend. Typing them means a payload
//! under the wrong field name fails at the boundary; a raw lookup under the
//! wrong name reads as absent instead, which would leave the away timer off
//! or erase the recap row without saying anything.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Whether one terminal is looking at the session.
///
/// The host owns the away timer, so a terminal reports this fact and nothing
/// more: what an unfocused terminal means over time is the host's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusReport {
    /// Whether the session is on screen and in front in this terminal.
    pub focused: bool,
}

impl FocusReport {
    /// The params object this report travels as. A struct holding one
    /// boolean cannot fail to serialize.
    pub fn params(self) -> Value {
        serde_json::to_value(self).expect("a focus report serializes to an object")
    }
}

/// The recap text a client shows as it arrives.
///
/// The row itself is durable in the session log; this notice is the live
/// copy of the same text, so a client that was watching when the answer
/// landed does not wait for a replay to read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecapNotice {
    /// The recap sentence the host wrote to the log.
    pub text: String,
}

impl RecapNotice {
    /// The params object this notice travels as. A struct holding one
    /// string cannot fail to serialize.
    pub fn params(self) -> Value {
        serde_json::to_value(self).expect("a recap notice serializes to an object")
    }
}

#[cfg(test)]
mod tests;
