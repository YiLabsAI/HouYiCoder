//! The gate's answer to a persisted rule set it could not read. The unread
//! file may hold a deny rule, so the rules left in memory are not the whole
//! rule set: anything short of a deny becomes a safety ask, which stored
//! consent cannot cover and which a headless session turns into a refusal.

use std::sync::Mutex;

use crate::decision::{AskReason, AskSource, Decision};
use crate::store::StoreError;

/// Identifier every decision produced by this rule carries as its validator,
/// stable for the metrics buckets and the audit record.
pub(crate) const STORE_UNREADABLE: &str = "store_unreadable";

/// Why the persisted rule set could not be read, when it could not. An empty
/// value means the store was read, or no store was attached.
#[derive(Default)]
pub(crate) struct StoreFailure(Mutex<Option<String>>);

impl StoreFailure {
    /// Record a failed read with the repair instruction the user needs, and
    /// send it to diagnostics once, so the startup notice and the ask that
    /// follows carry the same sentence.
    pub(crate) fn record(&self, error: &StoreError) {
        let detail = format!(
            "{error}. Repair or delete that file and restart; until then no stored rule can be trusted, so every tool call asks for approval"
        );
        tracing::warn!("[permission] {detail}");
        *self.0.lock().expect("mode gate mutex") = Some(detail);
    }

    /// The recorded reason, for the startup notice that names the file.
    pub(crate) fn reason(&self) -> Option<String> {
        self.0.lock().expect("mode gate mutex").clone()
    }

    /// Replace anything short of a deny with the safety ask. A deny stays a
    /// deny: turning it into an ask would offer the user a yes the old verdict
    /// never allowed.
    pub(crate) fn escalate(&self, decided: Decision) -> Decision {
        if matches!(&decided, Decision::Deny(_)) {
            return decided;
        }
        match self.reason() {
            Some(detail) => Decision::Ask(AskReason {
                source: AskSource::SystemSafety,
                validator: STORE_UNREADABLE,
                detail,
                containment_note: None,
            }),
            None => decided,
        }
    }
}
