//! The manager's control entry points: starting the configured servers,
//! asking one to reconnect, and flipping one's runtime enable switch.
//! Everything here changes what runs; the views read, the supervisor owns
//! the loop these entries poke.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use houyicoder_protocol::mcp::{McpConfigScope, McpConnectionState, ScopedMcpServerConfig};
use tokio::sync::mpsc;

use super::McpConnections;
use super::ServerSlot;
use super::StderrTail;

impl McpConnections {
    /// Start every configured server as its own supervised task. A disabled
    /// server is recorded as disabled and its supervisor parks instead of
    /// launching: the trust gate marks an untrusted workspace's project
    /// entries this way, and no process or request may follow from one. The
    /// trust answer travels with the list it gated, so a later enable request
    /// is judged by the same trust the start was.
    pub async fn start_all(&self, servers: Vec<ScopedMcpServerConfig>, trusted: bool) {
        for config in servers {
            let name = config.name.clone();
            let disabled = config.disabled;
            let locked = disabled && config.scope == McpConfigScope::Project && !trusted;
            let (wake_tx, wake_rx) = mpsc::unbounded_channel();
            {
                let mut slots = self.slots.write().await;
                slots.insert(
                    name.clone(),
                    ServerSlot {
                        state: if disabled {
                            McpConnectionState::Disabled {
                                reason: if locked {
                                    "the workspace is not trusted".into()
                                } else {
                                    "disabled by configuration".into()
                                },
                            }
                        } else {
                            McpConnectionState::Pending
                        },
                        config,
                        tools: Vec::new(),
                        resources: Vec::new(),
                        instructions: None,
                        server_version: None,
                        stderr: StderrTail::default(),
                        peer: None,
                        wake: Some(wake_tx),
                        roots_served: Arc::new(AtomicBool::new(false)),
                        enabled: !disabled,
                        locked,
                    },
                );
            }
            let manager = self.clone();
            let task = tokio::spawn(async move {
                manager.supervise(name, wake_rx).await;
            });
            if let Ok(mut tasks) = self.tasks.lock() {
                tasks.push(task);
            }
        }
    }

    /// Ask one server to reconnect now, whether it failed or the user wants a
    /// fresh handshake.
    pub async fn reconnect(&self, name: &str) {
        let slots = self.slots.read().await;
        if let Some(wake) = slots.get(name).and_then(|slot| slot.wake.clone()) {
            // A closed receiver means the supervisor is gone: nothing is
            // waiting and nothing needs to be woken.
            let _ = wake.send(());
        }
    }

    /// Flip one server's runtime switch. Disabling drops its peer and
    /// everything it provided, so the tool source stops advertising it the
    /// moment the user turns it off, and the supervisor parks until the
    /// switch flips back. Enabling a server an untrusted workspace locked is
    /// refused: the trust gate must not be walkable from a pane key.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        let wake = {
            let mut slots = self.slots.write().await;
            let slot = slots
                .get_mut(name)
                .ok_or_else(|| format!("no server named {name} is configured"))?;
            if enabled && slot.locked {
                return Err(format!("{name} stays off until its workspace is trusted"));
            }
            if slot.enabled == enabled {
                return Ok(());
            }
            slot.enabled = enabled;
            if enabled {
                slot.state = McpConnectionState::Pending;
            } else {
                slot.peer = None;
                slot.tools.clear();
                slot.resources.clear();
                slot.instructions = None;
                slot.state = McpConnectionState::Disabled {
                    reason: "disabled by the user".into(),
                };
            }
            slot.wake.clone()
        };
        if !enabled {
            self.announce_change();
        }
        if let Some(wake) = wake {
            // Unbounded, so the signal lands even while the supervisor is
            // inside a handshake that cannot read it yet; a closed receiver
            // means the supervisor is gone and nothing waits.
            let _ = wake.send(());
        }
        Ok(())
    }

    /// Whether the slot's runtime switch is on. A slot the map no longer
    /// holds counts as off, so a supervisor between removal and shutdown
    /// parks instead of reading a half state.
    pub(super) async fn slot_enabled(&self, name: &str) -> bool {
        self.slots
            .read()
            .await
            .get(name)
            .is_some_and(|slot| slot.enabled)
    }
}
