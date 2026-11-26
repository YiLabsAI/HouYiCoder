//! Document and diagnostics queries over one established connection: open a
//! file without disturbing a synchronized version, and wait for the
//! diagnostics a write triggers.

use std::path::Path;
use std::sync::Arc;

use houyicoder_config as config;
use houyicoder_protocol::lsp::LspDiagnostic;
use tokio::time::Instant;
use tokio::time::sleep_until;

use super::LspConnections;
use super::PublishedDiagnostics;
use super::watch;
use crate::child_process::file_uri;
use crate::child_process::path_from_uri;

/// The diagnostics-store key for one file URI. A workspace opened through a
/// symlink can produce two spellings of the same file: the caller's and the
/// server's resolved one (macOS /var to /private/var). The key resolves the
/// path behind the URI when it exists, so both sides land in one entry; a
/// URI whose path cannot be resolved keeps its spelling. The protocol URIs
/// sent to the server are untouched.
pub(super) fn diagnostic_key(uri: &str) -> String {
    let Some(path) = path_from_uri(uri) else {
        return uri.to_string();
    };
    match dunce::canonicalize(&path) {
        Ok(resolved) => file_uri(&resolved),
        Err(_) => uri.to_string(),
    }
}

impl LspConnections {
    /// The diagnostics the last publish left for one file on one server.
    pub async fn file_diagnostics(&self, name: &str, uri: &str) -> Vec<LspDiagnostic> {
        self.slots
            .read()
            .await
            .get(name)
            .and_then(|slot| {
                slot.diagnostics.lock().ok().and_then(|map| {
                    map.get(&diagnostic_key(uri))
                        .map(|entry| entry.findings.clone())
                })
            })
            .unwrap_or_default()
    }

    /// Open one document if it is not open yet, without touching the
    /// version of a document already synchronized. A read-only query on an
    /// unchanged file has nothing to report to the server. None means no
    /// server routes the suffix; an error names a routed server whose
    /// connection went away between the start and the open.
    pub async fn ensure_document(
        self: &Arc<Self>,
        path: &Path,
        content: &str,
    ) -> Result<Option<String>, String> {
        let Some(name) = self.server_for_path(path).await else {
            return Ok(None);
        };
        drop(self.ensure_started(&name).await?);
        let slot = self
            .slots
            .read()
            .await
            .get(&name)
            .cloned()
            .ok_or_else(|| format!("no language server named {name} is configured"))?;
        let language = config::language_for_extension(&slot.config, &watch::extension_of(path))
            .unwrap_or("plaintext")
            .to_string();
        let uri = file_uri(path);
        let mut live = slot.live.lock().await;
        let connection = live
            .as_mut()
            .ok_or_else(|| format!("{name} is not connected"))?;
        if !connection.open_docs.contains_key(&uri) {
            connection.client.notify(
                "textDocument/didOpen",
                serde_json::json!({
                    "textDocument": {
                        "uri": uri.clone(),
                        "languageId": language,
                        "version": 1,
                        "text": content,
                    },
                }),
            )?;
            connection.open_docs.insert(uri, 1);
        }
        Ok(Some(name))
    }

    /// How many publishes one file has received so far, and zero when it
    /// has none yet. A caller reads this before a write and passes it to the
    /// wait, so only a publish for that same file after the write can
    /// satisfy it; a publish for another open file changes nothing here.
    pub async fn publish_generation(&self, name: &str, uri: &str) -> u64 {
        self.slots
            .read()
            .await
            .get(name)
            .and_then(|slot| {
                slot.diagnostics
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&diagnostic_key(uri)).map(|entry| entry.generation))
            })
            .unwrap_or(0)
    }

    /// Wait for a publish for this file that arrived after the given
    /// generation, and return its findings then; None means no publish
    /// arrived in time, which is unknown rather than clean. The wait ends
    /// at the grace mark unless the server reports indexing, in which case
    /// it extends to the cap. The findings a previous publish left behind
    /// are never returned: a stale error reported as current would tell the
    /// model a fixed problem is still there.
    pub async fn wait_for_diagnostics(
        &self,
        name: &str,
        uri: &str,
        since: u64,
    ) -> Option<Vec<LspDiagnostic>> {
        let slot = self.slots.read().await.get(name).cloned()?;
        let grace_deadline = Instant::now() + self.timings.diagnostics_grace;
        let cap_deadline = Instant::now() + self.timings.diagnostics_cap;
        loop {
            // The waiter registers before the store is read, so a publish
            // cannot slip between the check and the wait: notify_waiters
            // stores no permit, and only a future created before the notify
            // is guaranteed the wake.
            let notified = slot.diag_notify.notified();
            tokio::pin!(notified);
            if let Some(entry) = Self::published(&slot, uri)
                && entry.generation > since
            {
                return Some(entry.findings);
            }
            let indexing = {
                let live = slot.live.lock().await;
                live.as_ref()
                    .is_some_and(|connection| connection.client.is_indexing())
            };
            let deadline = if indexing {
                cap_deadline
            } else {
                grace_deadline
            };
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            tokio::select! {
                _ = notified => {}
                _ = sleep_until(deadline) => return None,
            }
        }
    }

    /// One file's current entry, cloned out from under the store lock.
    fn published(slot: &super::ServerSlot, uri: &str) -> Option<PublishedDiagnostics> {
        slot.diagnostics
            .lock()
            .ok()
            .and_then(|map| map.get(&diagnostic_key(uri)).cloned())
    }
}
