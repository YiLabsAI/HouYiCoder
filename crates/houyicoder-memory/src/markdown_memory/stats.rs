//! Advisory sidecar counters for recall hits, gate violations, and occurrences.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_api::memory::MemoryWriteOutcome;
use houyicoder_context::MemoryRecallStats;

use super::MarkdownMemoryProvider;
use super::io::{StatRecord, sanitize_key, scopes_for_key_impl, write_bytes_advisory};
use super::lock::RootLock;

/// Advisory recall-frequency sidecar filename in the write root.
pub(super) const STATS_FILE: &str = ".stats.json";

impl MarkdownMemoryProvider {
    pub(super) fn stats_path(&self) -> PathBuf {
        self.write_root().join(STATS_FILE)
    }

    pub(super) fn load_stats(&self) -> HashMap<String, StatRecord> {
        let Ok(text) = fs::read_to_string(self.stats_path()) else {
            return HashMap::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub(super) fn save_stats(&self, stats: &HashMap<String, StatRecord>) {
        let Ok(json) = serde_json::to_string(stats) else {
            return;
        };
        if write_bytes_advisory(&self.stats_path(), json.as_bytes()).is_err() {
            tracing::warn!("memory recall stats could not be persisted (advisory)");
        }
    }

    pub(super) fn record_occurrence(&self, key: &str, outcome: MemoryWriteOutcome) {
        if !outcome.changed() {
            return;
        }
        let mut stats = self.load_stats();
        let stat = stats.entry(key.to_string()).or_default();
        match outcome {
            MemoryWriteOutcome::Created => stat.occurrences = stat.occurrences.max(1),
            MemoryWriteOutcome::Updated => stat.occurrences = stat.occurrences.saturating_add(1),
            MemoryWriteOutcome::Unchanged => {}
        }
        self.save_stats(&stats);
    }

    pub(super) fn read_recall_stats_impl(&self) -> Vec<MemoryRecallStats> {
        self.load_stats()
            .into_iter()
            .map(|(key, r)| MemoryRecallStats {
                key,
                recall_hits: r.recall_hits,
                gate_violations: r.gate_violations,
                occurrences: r.occurrences,
                last_access_ts: r.last_access_ts,
            })
            .collect()
    }

    pub(super) fn record_recall_hits_impl(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let Ok(_guard) = self.write_lock.try_lock() else {
            tracing::debug!("memory recall stats write skipped: writer busy (advisory)");
            return;
        };
        let _root_lock = match RootLock::try_acquire(self.write_root()) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                tracing::debug!("memory recall stats write skipped: root busy (advisory)");
                return;
            }
            Err(_) => {
                tracing::warn!("memory recall stats write skipped: root lock unavailable");
                return;
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut stats = self.load_stats();
        for key in keys {
            let r = stats.entry(key.clone()).or_default();
            r.recall_hits = r.recall_hits.saturating_add(1);
            r.last_access_ts = now;
        }
        self.save_stats(&stats);
    }

    pub(super) fn record_gate_violation_impl(&self, key: &str) {
        let Ok(key) = sanitize_key(key) else {
            return;
        };
        let Ok(_guard) = self.write_lock.try_lock() else {
            tracing::debug!("memory gate stats write skipped: writer busy (advisory)");
            return;
        };
        if scopes_for_key_impl(self, &key).is_empty() {
            return;
        }
        let _root_lock = match RootLock::try_acquire(self.write_root()) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                tracing::debug!("memory gate stats write skipped: root busy (advisory)");
                return;
            }
            Err(_) => {
                tracing::warn!("memory gate stats write skipped: root lock unavailable");
                return;
            }
        };
        let mut stats = self.load_stats();
        let r = stats.entry(key).or_default();
        r.gate_violations = r.gate_violations.saturating_add(1);
        self.save_stats(&stats);
    }
}
