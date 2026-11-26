//! The management verbs over one manager: end every server with the session,
//! flip one switch, and restart one server. Each verb stops a live
//! connection through the same helper, so the shutdown sequence (clear the
//! diagnostics, ask the server to exit inside its timeout, drop the child)
//! has one spelling.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::executor::block_on;
use houyicoder_protocol::lsp::LspServerState;
use tokio::task::spawn_blocking;
use tokio::time::timeout;

use super::LspConnections;
use super::ServerSlot;
use super::clear_diagnostics;

impl LspConnections {
    /// Stop every server gracefully and join their children.
    pub async fn shutdown(&self) {
        // Set before anything is taken, so a watcher waking from a backoff
        // sleep cannot launch a replacement for a session that ended.
        self.closed.store(true, Ordering::Relaxed);
        let slots: Vec<Arc<ServerSlot>> = self.slots.read().await.values().cloned().collect();
        for slot in slots {
            stop_connection(&slot).await;
            *slot.state.write().await = LspServerState::Idle;
        }
    }

    /// Flip one server's switch. Disabling stops the live connection; the
    /// next ask starts a fresh one only when the switch is back on.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        let slot = self
            .slots
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no language server named {name} is configured"))?;
        if enabled && slot.locked {
            return Err(format!("{name} stays off until its workspace is trusted"));
        }
        if slot.enabled.load(Ordering::Relaxed) == enabled {
            return Ok(());
        }
        slot.enabled.store(enabled, Ordering::Relaxed);
        if enabled {
            *slot.state.write().await = LspServerState::Idle;
            return Ok(());
        }
        stop_connection(&slot).await;
        *slot.state.write().await = LspServerState::Disabled {
            reason: "disabled by the user".into(),
        };
        Ok(())
    }

    /// Stop one server and start it again.
    pub async fn restart(self: &Arc<Self>, name: &str) -> Result<(), String> {
        let slot = self
            .slots
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no language server named {name} is configured"))?;
        stop_connection(&slot).await;
        *slot.state.write().await = LspServerState::Idle;
        drop(self.ensure_started(name).await?);
        Ok(())
    }
}

/// Drop the diagnostics a connection published and stop it inside the
/// server's own shutdown timeout. After the exit notification the protocol
/// asks the client to wait for the server to stop on its own; the bounded
/// wait gives a well-behaved server that chance before the handle drop
/// terminates its group.
async fn stop_connection(slot: &ServerSlot) {
    clear_diagnostics(slot);
    let connection = slot.live.lock().await.take();
    if let Some(connection) = connection {
        let budget = Duration::from_millis(slot.config.shutdown_timeout_ms);
        connection.client.shutdown(budget).await;
        if let Some(child) = connection.child {
            // The launcher's exit waiter calls a blocking process wait, and
            // the exit notification it is waiting on is flushed by a task on
            // this same runtime. Waiting on a worker thread would starve that
            // task and deadlock a single-threaded runtime, so the wait runs
            // on the blocking pool.
            let waiter = child.wait();
            drop(timeout(budget, spawn_blocking(move || block_on(waiter))).await);
        }
    }
}
