//! The agent-side adapter: speaks the base JSON-RPC dialect so a stock
//! client drives the engine. Dispatch is a pure function over an inbound
//! request — the IO bridge hands requests in and ships responses out, so
//! routing and the carrier stay separable. This file owns routing + typed
//! handlers; the carrier is whichever transport the composition root pins.
//!
//! Method names mirror ACP v1 exactly: the client sends session/new,
//! session/load, session/cancel, session/prompt; the agent emits
//! session/update and the session/request_permission reverse request. The
//! acpx/session/takeControl ext verb rides the base ext_method axis as
//! _acpx/session/takeControl (the leading underscore is the base protocol's
//! ext-method convention). The runner-abort half of cancel/takeControl-force
//! lands with the IO bridge (which owns the Arc<Runner>); the store-only
//! first cut here is correct because no run can be live until prompt ships.

use houyicoder_async::PFut;
use houyicoder_context::{EventId, SessionId};
use houyicoder_protocol::acp_wire::{
    AcpErrorCode, AcpNotification, AcpRequest, AcpResponse, AgentCapabilities, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
};
use houyicoder_protocol::acpx::{AcpxCapabilities, TakeControlOutcome, TakeControlParams};

use crate::lifecycle::LifecycleError;
use crate::lifecycle::{Lifecycle, SessionLeaseStore, SessionRecord};
#[cfg(test)]
use crate::lifecycle::{PendingPermission, PendingTurn};
use serde_json::Map;

/// The adapter's static config: the capability block it advertises at
/// initialize. Built by the composition root from what the agent supports.
pub struct AcpAdapter {
    caps: AcpxCapabilities,
    protocol_version: u16,
    store: SessionLeaseStore,
}

impl AcpAdapter {
    /// Build an adapter that advertises the given capabilities at the numeric
    /// protocol version (V1 = 1) and tracks session lifecycle in the store.
    pub fn new(caps: AcpxCapabilities, protocol_version: u16, store: SessionLeaseStore) -> Self {
        Self {
            caps,
            protocol_version,
            store,
        }
    }

    /// Route one inbound request to its handler. Unknown methods reply
    /// method-not-found. Handlers delegate to the async lifecycle store, so
    /// dispatch returns a PFut the IO bridge awaits. The method strings are
    /// the ACP v1 names (session/new, session/load); the ext verb carries
    /// the leading underscore the base protocol's ext_method axis requires.
    pub fn handle<'a>(&'a self, req: &'a AcpRequest) -> PFut<'a, AcpResponse> {
        match req.method.as_str() {
            "initialize" => Box::pin(async move { self.handle_initialize(req) }),
            "session/new" => Box::pin(async move { self.handle_new_session(req) }),
            "session/load" => Box::pin(async move { self.handle_load_session(req).await }),
            "_acpx/session/takeControl" => {
                Box::pin(async move { self.handle_take_control(req).await })
            }
            _ => Box::pin(async move {
                AcpResponse::err(
                    req.id.clone(),
                    AcpErrorCode::MethodNotFound,
                    format!("unknown method: {}", req.method),
                )
            }),
        }
    }

    /// Route one inbound notification (no id, no reply). The base protocol
    /// carries session/cancel as a notification — the adapter reaps the
    /// lifecycle state and (once the IO bridge lands) aborts the live run.
    /// Unknown notifications are dropped silently per JSON-RPC 2.0.
    pub fn handle_notification<'a>(&'a self, notif: &'a AcpNotification) -> PFut<'a, ()> {
        match notif.method.as_str() {
            "session/cancel" => Box::pin(async move { self.handle_cancel(notif).await }),
            _ => Box::pin(async {}),
        }
    }

    fn handle_initialize(&self, req: &AcpRequest) -> AcpResponse {
        let mut meta = Map::new();
        meta.insert(
            "acpx".into(),
            serde_json::to_value(&self.caps).expect("capabilities serialize"),
        );
        let resp = InitializeResponse {
            protocol_version: self.protocol_version,
            agent_capabilities: AgentCapabilities {
                load_session: self.caps.detach,
                ..Default::default()
            },
            auth_methods: Vec::new(),
            agent_info: None,
            meta: Some(meta),
        };
        AcpResponse::ok(
            req.id.clone(),
            serde_json::to_value(resp).expect("initialize response serialize"),
        )
    }

    fn handle_new_session(&self, req: &AcpRequest) -> AcpResponse {
        let _params: NewSessionRequest = match req.params.as_ref() {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        format!("bad params: {e}"),
                    );
                }
            },
            None => {
                return AcpResponse::err(
                    req.id.clone(),
                    AcpErrorCode::InvalidParams,
                    "missing params",
                );
            }
        };
        let sid = SessionId::new();
        self.store.insert(SessionRecord {
            session_id: sid,
            event_cursor: EventId::new(),
            pending: None,
            runner_checkpoint: Vec::new(),
            lease_holder: Some("new_session".into()),
        });
        let resp = NewSessionResponse {
            session_id: sid.to_string(),
            meta: None,
        };
        AcpResponse::ok(
            req.id.clone(),
            serde_json::to_value(resp).expect("new_session response serialize"),
        )
    }

    fn handle_load_session<'a>(&'a self, req: &'a AcpRequest) -> PFut<'a, AcpResponse> {
        let params: LoadSessionRequest = match req.params.as_ref() {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return Box::pin(async move {
                        AcpResponse::err(
                            req.id.clone(),
                            AcpErrorCode::InvalidParams,
                            format!("bad params: {e}"),
                        )
                    });
                }
            },
            None => {
                return Box::pin(async move {
                    AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        "missing params",
                    )
                });
            }
        };
        let sid = match SessionId::from_display_string(&params.session_id) {
            Some(s) => s,
            None => {
                return Box::pin(async move {
                    AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        "bad session id",
                    )
                });
            }
        };
        let store = &self.store;
        Box::pin(async move {
            match store.load_session(sid).await {
                Ok(_) => AcpResponse::ok(
                    req.id.clone(),
                    serde_json::to_value(LoadSessionResponse::default())
                        .expect("load_session response serialize"),
                ),
                Err(_) => AcpResponse::err(
                    req.id.clone(),
                    AcpErrorCode::ResourceNotFound,
                    "session not found",
                ),
            }
        })
    }

    fn handle_take_control<'a>(&'a self, req: &'a AcpRequest) -> PFut<'a, AcpResponse> {
        let params: TakeControlParams = match req.params.as_ref() {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return Box::pin(async move {
                        AcpResponse::err(
                            req.id.clone(),
                            AcpErrorCode::InvalidParams,
                            format!("bad params: {e}"),
                        )
                    });
                }
            },
            None => {
                return Box::pin(async move {
                    AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        "missing params",
                    )
                });
            }
        };
        let sid = match SessionId::from_display_string(&params.session_id) {
            Some(s) => s,
            None => {
                return Box::pin(async move {
                    AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        "bad session id",
                    )
                });
            }
        };
        let store = &self.store;
        Box::pin(async move {
            let outcome = match store.take_control(sid, params.force).await {
                Ok(()) => {
                    // Detect a parked pending permission turn: if one exists,
                    // signal it so the new lease holder knows to expect a
                    // re-emit. The actual re-send over the IO bridge is the
                    // activation step (lands when a persistent transport
                    // drives this adapter with a parked turn); this scaffold
                    // proves the detection at the adapter boundary so the
                    // mechanism is not dead code.
                    let has_pending = store.pending(sid).is_some();
                    TakeControlOutcome::Granted {
                        pending_resent: if has_pending { Some(true) } else { None },
                    }
                }
                Err(LifecycleError::NotFound) => TakeControlOutcome::Denied {
                    reason: "session not found".into(),
                },
                Err(LifecycleError::LeaseHeld(holder)) => TakeControlOutcome::Denied {
                    reason: format!("lease held by {holder}"),
                },
                Err(_) => TakeControlOutcome::Denied {
                    reason: "lifecycle error".into(),
                },
            };
            AcpResponse::ok(
                req.id.clone(),
                serde_json::to_value(outcome).expect("take_control outcome serialize"),
            )
        })
    }

    async fn handle_cancel(&self, notif: &AcpNotification) {
        // session/cancel is a notification: no id, no reply. The params carry
        // sessionId. The store reap (clears pending + state Cancelled) runs
        // now; the runner abort lands with the IO bridge. A bad or missing
        // session id is dropped silently — a notification has no reply path.
        let sid = notif
            .params
            .as_ref()
            .and_then(|v| v.get("sessionId"))
            .and_then(|v| v.as_str())
            .and_then(SessionId::from_display_string);
        let Some(sid) = sid else {
            return;
        };
        drop(self.store.cancel(sid).await);
    }
}

#[cfg(test)]
mod tests;
