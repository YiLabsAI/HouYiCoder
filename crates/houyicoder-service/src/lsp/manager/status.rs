//! What the status views read: the routing table's answers and the
//! per-server rows the pane and the CLI render.

use std::iter::once;
use std::path::Path;

use houyicoder_protocol::lsp::LspConfigScope;
use houyicoder_protocol::lsp::LspServerState;
use houyicoder_protocol::lsp::LspServerStatusView;

use super::LspConnections;

impl LspConnections {
    /// The server that owns one path's suffix, if the table routes it.
    pub async fn server_for_path(&self, path: &Path) -> Option<String> {
        let extension = format!(
            ".{}",
            path.extension()
                .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default()
        );
        self.routing.read().await.get(&extension).cloned()
    }

    /// The status rows the views render, ordered by name.
    pub async fn statuses(&self) -> Vec<LspServerStatusView> {
        let slots = self.slots.read().await;
        let mut rows = Vec::with_capacity(slots.len());
        for (name, slot) in slots.iter() {
            let base = slot.state.read().await.clone();
            let live = slot.live.lock().await;
            let state = match (&base, live.as_ref()) {
                (LspServerState::Running, Some(connection)) if connection.client.is_indexing() => {
                    LspServerState::Indexing {
                        message: connection.client.index_message(),
                    }
                }
                _ => base,
            };
            let open_documents = live
                .as_ref()
                .map(|connection| connection.open_docs.len())
                .unwrap_or(0);
            let diagnostic_count = slot
                .diagnostics
                .lock()
                .map(|diagnostics| diagnostics.values().map(|entry| entry.findings.len()).sum())
                .unwrap_or(0);
            rows.push(LspServerStatusView {
                name: name.clone(),
                scope: slot.config.scope,
                state,
                locked: slot.locked,
                command_line: once(slot.config.command.clone())
                    .chain(slot.config.args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" "),
                extensions: slot.config.extensions(),
                open_documents,
                diagnostic_count,
                recent_stderr: slot
                    .stderr
                    .lock()
                    .map(|tail| tail.lines())
                    .unwrap_or_default(),
            });
        }
        rows
    }

    /// The scope one server was configured from, for a receipt that must say
    /// whether a change outlives the session.
    pub async fn config_scope(&self, name: &str) -> Option<LspConfigScope> {
        self.slots
            .read()
            .await
            .get(name)
            .map(|slot| slot.config.scope)
    }
}
