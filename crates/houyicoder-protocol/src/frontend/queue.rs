//! Typed identity for user input crossing the frontend queue boundary.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

static NEXT_PENDING_INPUT_ID: AtomicU64 = AtomicU64::new(1);

/// Stable identity for one pending user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PendingInputId(pub u64);

impl PendingInputId {
    /// Mint an identity unique within the current frontend process.
    pub fn fresh() -> Self {
        Self(NEXT_PENDING_INPUT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// One identified user input carried between the frontend and runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueuedInput {
    /// Identity retained across enqueue, commit, and removal messages.
    pub id: PendingInputId,
    /// User-authored input.
    pub text: String,
}

impl QueuedInput {
    /// Create an identified input from user-provided text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            id: PendingInputId::fresh(),
            text: text.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum QueuedInputRepr {
    Identified { id: PendingInputId, text: String },
    Legacy(String),
}

impl<'de> Deserialize<'de> for QueuedInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match QueuedInputRepr::deserialize(deserializer)? {
            QueuedInputRepr::Identified { id, text } => Self { id, text },
            QueuedInputRepr::Legacy(text) => Self {
                id: PendingInputId(0),
                text,
            },
        })
    }
}

impl PartialEq<String> for QueuedInput {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl PartialEq<str> for QueuedInput {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl From<String> for QueuedInput {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<&str> for QueuedInput {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}
