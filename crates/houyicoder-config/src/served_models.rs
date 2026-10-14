//! The provider-served model cache: a one-shot GET /v1/models snapshot
//! written by the provider crate at startup (fire-and-forget, fault-
//! tolerant) and read here synchronously to check whether a model id the
//! user configured is actually served by the provider. Sync file read,
//! longest-id-first match, no-cache => no existence check (cannot know, so
//! do not warn).
//!
//! Existence is narrow on purpose: the OpenAI-compatible /v1/models returns
//! only model ids, not capability fields. context_window / max_output stay
//! on the existing family-table + per-model override + the rejected-request
//! learner chain — /v1/models does not touch them.
//!
//! Not memoized: the only callers are config-load (startup, /model Info,
//! /status) — per-command, not per-frame — so a fresh file read per call is
//! cheap and keeps the cache testable (a process-global memoize would let
//! one test cache file shadow another).

use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::env::temp_dir;
use std::fs::read_to_string;
#[cfg(test)]
use std::fs::remove_file;
#[cfg(test)]
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::process::id;

/// The on-disk cache: the ids the provider said it serves, plus the fetch
/// timestamp. Written 0o600 by the provider; read here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServedModels {
    pub ids: Vec<String>,
    pub timestamp: u64,
}

/// The cache file path: the config-home cache dir, file served-models.json.
pub fn cache_path() -> PathBuf {
    crate::config_home()
        .join("cache")
        .join("served-models.json")
}

/// Load the cached ids. A missing or corrupt file yields an empty vec —
/// callers treat empty as "cannot check existence, skip the warning" rather
/// than "no model is served". Under test builds this returns empty without
/// touching disk: config unit tests would otherwise read a real-HOME cache
/// file a production run wrote, contaminating their no-warning assertions.
/// The served-check itself is tested via validate_catalog's explicit served
/// param; the production read→warn path is covered by a PTY journey on the
/// real binary.
pub fn cached_ids() -> Vec<String> {
    #[cfg(not(test))]
    {
        load_ids_at(&cache_path())
    }
    #[cfg(test)]
    {
        Vec::new()
    }
}

/// Read the cache at an explicit path. Returns the ids or empty on any
/// error (missing file, corrupt JSON). Public so the provider crate (which
/// writes the cache) and tests share one reader.
pub fn load_ids_at(path: &Path) -> Vec<String> {
    match read_to_string(path) {
        Ok(raw) => serde_json::from_str::<ServedModels>(&raw)
            .map(|p| p.ids)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Whether a model id is in the served list. Exact match first, then
/// longest-id-first substring match, so qwen3.7-max matches a served qwen3
/// entry. Empty cache => false (cannot confirm; callers gate the warning on
/// cache presence, not on this bool, to avoid warning when we simply do not
/// know).
pub fn served_model_exists(model: &str) -> bool {
    exists_in(&cached_ids(), model)
}

/// Path/list-explicit variant for tests: does the model appear in the given
/// id list? Same match rule (exact then longest-id-first substring).
pub fn exists_in(ids: &[String], model: &str) -> bool {
    let m = model.to_lowercase();
    if ids.iter().any(|id| id.eq_ignore_ascii_case(model)) {
        return true;
    }
    // Longest-id-first so the most specific served id wins the substring
    // match (avoids a short id shadowing a longer one).
    let mut sorted: Vec<&String> = ids.iter().collect();
    sorted.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    sorted.iter().any(|id| m.contains(&id.to_lowercase()))
}

#[cfg(test)]
mod tests;
