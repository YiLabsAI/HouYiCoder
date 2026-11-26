//! What the status views read: the route one suffix resolves to and the
//! per-server rows the pane and the CLI render.

use std::iter::once;
use std::path::Path;
use std::sync::atomic::Ordering;

use houyicoder_protocol::lsp::LspConfigScope;
use houyicoder_protocol::lsp::LspServerState;
use houyicoder_protocol::lsp::LspServerStatusView;
use houyicoder_protocol::lsp::LspTransportKind;

use super::LspConnections;
use super::ServerSlot;

impl LspConnections {
    /// The server one path's suffix resolves to. The answer is computed from
    /// the slots at this instant, so a switch or a disable changes the route
    /// without a table rebuild. Only an enabled stdio slot
    /// that declares the suffix is a candidate; ties follow the explicit
    /// scope order (project, user, product plugin, frontend plugin cache)
    /// and then the server name, so the user's own entry always outranks a
    /// plugin's and a locked project entry can never hijack a suffix.
    pub async fn server_for_path(&self, path: &Path) -> Option<String> {
        let extension = format!(
            ".{}",
            path.extension()
                .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default()
        );
        let slots = self.slots.read().await;
        slots
            .values()
            .filter(|slot| Self::routes(slot, &extension))
            .min_by_key(|slot| (scope_rank(slot.config.scope), slot.config.name.clone()))
            .map(|slot| slot.config.name.clone())
    }

    /// Whether one slot may answer a query for this suffix.
    fn routes(slot: &ServerSlot, extension: &str) -> bool {
        slot.enabled.load(Ordering::Relaxed)
            && slot.config.transport == LspTransportKind::Stdio
            && slot
                .config
                .extension_to_language
                .keys()
                .any(|known| known.eq_ignore_ascii_case(extension))
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

/// The route precedence of one scope: the workspace's own file wins, then
/// the user's settings, then a product plugin, then the frontend's plugin
/// cache. Spelled out rather than derived from the enum's declaration order,
/// which lists the user scope first.
fn scope_rank(scope: LspConfigScope) -> u8 {
    match scope {
        LspConfigScope::Project => 0,
        LspConfigScope::User => 1,
        LspConfigScope::Plugin => 2,
        LspConfigScope::ClaudePlugin => 3,
    }
}
