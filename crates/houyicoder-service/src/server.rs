//! Protocol server — the service role of the protocol connection (not a
//! standalone process; the process entry is the cli composition root, which
//! assembles the runner + this server + the client). Performs the Hello
//! handshake, routes incoming requests to the runner, and pushes events back
//! to the client on the monotonic event stream. Holds the resume cursor so a
//! reconnecting client reports the last seq it processed and the server
//! replays the tail without re-sending the whole log.
//!
//! Layering (adjudicated 2026-07-16): the carrier abstraction the frontend
//! holds is a client-side concern. The service server reads raw frame I/O
//! directly — a futures mpsc channel pair for the in-memory carrier here, a
//! framed byte reader for pipes later. The shared contract is the protocol
//! frame format (NDJSON frames of protocol serde types), never the client
//! carrier trait, so the service never depends on the layer above it.

#![allow(dead_code)] // pub server type consumed by other crates and tests; locally unused

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tokio::sync::Notify;

use crate::composition::SessionHost;
use crate::projection::{project_run_error, project_run_result};

use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_protocol::envelope::{ClientFrame, RequestId, ResponsePayload};
use houyicoder_protocol::frontend::FrontendEventKind;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::handshake::{Hello, Negotiated, negotiate};
use houyicoder_protocol::wire::{WireError, WireErrorKind};

/// The live sink installer lives in a child module so this file stays
/// under the size gate.
mod live_sink;
pub use live_sink::install_live_sink;

/// The raw frame I/O lives in a child module so this file stays under the
/// size gate.
mod io;
pub use io::ServerIo;

/// Request dispatch (routing one frontend request to its handler) lives in a
/// child module so this file stays under the size gate.
mod dispatch;

/// The /debug request handler lives in a child module so this file stays
/// under the size gate.
mod debug_dispatch;

/// Status-snapshot sidecar attachers (env/config display fields + per-model
/// usage projection) live in a child module so dispatch stays under the size
/// gate.
mod status_wire;

/// Mid-run + between-run session-notification + permission-mode-cycle
/// helpers live in a child module so this file stays under the size gate.
mod notif;

/// The mid-run permission reverse-request (ask the human, record the verdict
/// audit, apply consent) lives in a child module so this file stays under the
/// size gate.
mod approval;
mod trust;

/// Frame emission (push durable events, send typed responses/events on the
/// seq stream) lives in a child module so this file stays under the size gate.
mod emit;

/// The protocol server. Owns the runner handle + the session the connection
/// drives, plus the monotonic event seq counter. serve runs the handshake
/// then the request loop until the client disconnects or a fatal frame error
/// surfaces.
pub struct Server {
    runner: Arc<Runner>,
    session: SessionId,
    /// The monotonic event seq counter. Shared with the live delta sink (the
    /// runner fires deltas during a run; the sink fetch_adds from this same
    /// counter so live deltas and durable turn events share one monotonic seq
    /// stream a reconnecting client resumes from). Arc-atomic so the sink and
    /// the server share one source without a mutable borrow crossing the run
    /// future.
    next_seq: Arc<AtomicU64>,
    /// The next reverse-request id the server mints for a mid-turn permission
    /// ask. Distinct from the event seq so the two correlation axes never
    /// collide.
    next_req_id: u64,
    /// How many session events the server has already pushed to this client.
    /// A resumed run appends more events to the same log; without this cursor
    /// the replay-after-resume would re-send the whole log each loop. The
    /// trajectory mirror is append-ordered, so a count cursor skips exactly the
    /// pushed prefix. Invariant: the mirror must stay append-only (compaction
    /// appends a boundary event, never rewrites the prefix) or switch to a
    /// seq-id cursor.
    pushed_count: usize,
    /// The permission mode gate. The server reads current() + rules() for
    /// the /mode and /rules wire requests so the TUI does not import the
    /// permission crate directly.
    gate: Arc<dyn houyicoder_permission::ModeGate>,
    /// Sandbox session shared with the runner's tool provider. When set,
    /// the /permissions Workspace verbs mutate this fence so the next exec
    /// sees the new allow-paths. The gate's rule engine is a separate
    /// (tool allow/ask/deny) concern, not path access.
    sandbox_session: Option<Arc<dyn houyicoder_api::sandbox::SandboxSession>>,
    /// The session-indexed host a reattaching connection re-hydrates from.
    /// None on the single-shot path (serve behaves as before); Some when
    /// serve_session built this Server — the run path writes the parked
    /// PendingTurn, disconnect flushes pushed_count, serve-start re-emits.
    host: Option<Arc<SessionHost>>,
    /// The settings file path the /memory toggle handler persists to. Tests
    /// override it with a temp path so a flip never touches the real file.
    settings_path: std::path::PathBuf,
    /// The project workspace path the startup trust prompt gates on. None
    /// for a non-project session (a test harness, a home-dir run) so no
    /// prompt fires. Set at the composition root from the resolved cwd.
    project_path: Option<std::path::PathBuf>,
    /// Optional Append Notify shared with the runner's store: the store fires
    /// notify_one per append, this select's notified() branch wakes to drain
    /// mid-run so a tool-call frame ships while the run is still in flight
    /// (route B). None = the branch awaits a never-resolving pending future,
    /// so behavior is exactly today's (events push only at run resolve). The
    /// composition root shares one Arc<Notify> between the store impl + here.
    append_notify: Option<Arc<Notify>>,
    /// The session-metadata sidecar store, so the Status handler can attach
    /// the identity fields to the wire snapshot. None on the test path.
    meta_store: Option<Arc<dyn houyicoder_context::SessionMetaStore>>,
    /// The process-wide diagnostic sink's control handle. None when no sink
    /// was installed (the loader binary, tests). A /debug wire request
    /// against a server with no sink returns an error rather than silently
    /// succeeding — the user should know the sink is not there.
    diagnostics: Option<crate::diagnostics::DiagnosticsHandle>,
}

/// Reconnect-replay entry points (serve_session, resume_pending) live in a
/// child module to keep this file under the size gate.
pub(crate) mod session;

impl Server {
    /// Build a server with a fresh internal event-seq counter. Use
    /// new_with_shared_seq when a live delta sink must share the seq stream.
    pub fn new(
        runner: Arc<Runner>,
        session: SessionId,
        gate: Arc<dyn houyicoder_permission::ModeGate>,
    ) -> Self {
        Self::new_with_shared_seq(runner, session, gate, Arc::new(AtomicU64::new(0)))
    }

    /// Build a server bound to one runner + session + gate + the shared
    /// event-seq counter (also held by the live delta sink, so deltas and
    /// durable events share one monotonic stream).
    pub fn new_with_shared_seq(
        runner: Arc<Runner>,
        session: SessionId,
        gate: Arc<dyn houyicoder_permission::ModeGate>,
        next_seq: Arc<AtomicU64>,
    ) -> Self {
        Self {
            runner,
            session,
            next_seq,
            next_req_id: 0,
            pushed_count: 0,
            gate,
            sandbox_session: None,
            host: None,
            settings_path: houyicoder_config::settings_path(),
            project_path: None,
            append_notify: None,
            meta_store: None,
            diagnostics: crate::diagnostics::handle(),
        }
    }

    /// Override the settings path the /memory toggle handler persists to.
    /// Tests pass a temp path so a flip never writes the real settings file.
    pub fn with_settings_path(mut self, path: std::path::PathBuf) -> Self {
        self.settings_path = path;
        self
    }

    /// Set the project workspace path the startup trust prompt gates on.
    /// The composition root passes the resolved cwd so a project session
    /// prompts once before the run loop; a non-project session leaves it
    /// None and skips the prompt.
    pub fn with_project_path(mut self, path: std::path::PathBuf) -> Self {
        self.project_path = Some(path);
        self
    }

    /// Attach the sandbox session shared with the runner (the /permissions fence).
    pub fn with_session(
        mut self,
        session: Arc<dyn houyicoder_api::sandbox::SandboxSession>,
    ) -> Self {
        self.sandbox_session = Some(session);
        self
    }

    // The append-notify + meta-store attachment builders live in the session
    // child module to keep this file under the size gate; both are pub, so
    // callers are unaffected.

    /// Override the diagnostics handle the server read at construction. The
    /// construction path already reads the process-wide handle via
    /// diagnostics::handle(), so this is for tests that install a sink after
    /// building the server, or for explicitly clearing it (passing None) in
    /// a test that must reject /debug.
    pub fn with_diagnostics(
        mut self,
        handle: Option<crate::diagnostics::DiagnosticsHandle>,
    ) -> Self {
        self.diagnostics = handle;
        self
    }

    /// Mint a fresh reverse-request id for a mid-turn server-to-client ask.
    fn mint_req_id(&mut self) -> RequestId {
        let id = RequestId(self.next_req_id);
        self.next_req_id += 1;
        id
    }

    /// Write the pushed-event cursor back into the session host so a
    /// reattaching connection does not re-send the trajectory log the prior
    /// client already saw. No-op on the single-shot path (host None). Called
    /// at every disconnect return path so a mid-run or mid-permission
    /// disconnect retains the cursor alongside the parked PendingTurn.
    fn flush_pushed_count(&self) {
        if let Some(host) = &self.host {
            host.set_pushed_count(self.session, self.pushed_count);
        }
    }

    /// Apply a Yes-don't-ask consent when the client approves a tool call with
    /// scope "always". The server owns the approval (tool name + input Value
    /// the engine raised the interruption with), so the command is classified
    /// here — not in the frontend. git-checkpoint ops (git commit/rebase/reset/
    /// tag) route to a session-scope allow rule that shadows the builtin ask
    /// rule for that subcommand this session (cleared on restart, like the
    /// reference's session rule source); everything else becomes a durable
    /// allow rule. A compound/un-scopable bash command yields no rule
    /// (approved once only).
    fn apply_consent_rule(&self, tool_name: &str, input: &serde_json::Value) {
        let command = houyicoder_permission::input_content(tool_name, Some(input));
        if let Some(git_cmd) = houyicoder_permission::classify_git_op(tool_name, &command) {
            // The discard forms (force push, clean -fd, ...) return their
            // consent word too; only the four checkpoint subcommands map to a
            // shadow-able builtin rule. For a discard word, fall through to
            // the durable-rule path below (there is no builtin to shadow).
            if matches!(git_cmd, "commit" | "rebase" | "reset" | "tag") {
                use houyicoder_permission::{Effect, Rule, RuleContent, Scope};
                let rule = Rule::with_content(
                    "bash",
                    RuleContent::Prefix(format!("git {git_cmd}")),
                    Effect::Allow,
                )
                .unwrap()
                .with_scope(Scope::Session);
                self.gate.add_rule(rule);
                return;
            }
        }
        if let Some(rule) = crate::projection::consent_rule_for(tool_name, input) {
            self.gate.add_rule(rule);
        }
    }

    /// Build the /permission git reply: set the toggle when enabled is Some,
    /// then report the resulting state. The toggle now enables/disables the
    /// git-checkpoint builtin rules (no separate flag). Split out of the
    /// dispatch match so the set-then-report logic is unit-testable without
    /// driving the wire.
    fn ask_before_git_response(&self, enabled: Option<bool>) -> ResponsePayload {
        if let Some(e) = enabled {
            self.gate.set_git_checkpoint_enabled(e);
        }
        ResponsePayload::PermissionAskBeforeGit(self.gate.git_checkpoint_enabled())
    }

    /// Project the settings model section into the /model pane snapshot. A
    /// missing file or a malformed section yields defaults (no catalog), so
    /// the pane renders the empty-state guidance rather than failing.
    async fn handle_model_info(
        &mut self,
        io: &mut ServerIo,
        req_id: RequestId,
    ) -> Result<(), WireError> {
        let (section, _warnings) = houyicoder_config::load_model_section_from(&self.settings_path);
        let catalog = houyicoder_protocol::frontend::model::ModelCatalog {
            active_id: section.id,
            effort_level: section.effort_level,
            catalog: section
                .catalog
                .into_iter()
                .map(
                    |e| houyicoder_protocol::frontend::model::ModelCatalogEntry {
                        id: e.id,
                        display_name: e.display_name,
                        description: e.description,
                        effort: e.effort,
                    },
                )
                .collect(),
        };
        self.send_response(io, req_id, ResponsePayload::ModelInfo(catalog))
            .await
    }

    /// Run the connection: handshake, then receive request frames until the
    /// client closes. Each request is dispatched; events the run produces are
    /// pushed on the seq stream and the run outcome returns as a response on
    /// the req_id axis. Returns Ok for a clean close, Err for a wire-level
    /// failure the host surfaces. Owns the I/O so the host can spawn the whole
    /// loop onto a runtime.
    pub async fn serve(mut self, mut io: ServerIo) -> Result<(), WireError> {
        let _negotiated = self.handshake(&mut io).await?;
        // Ask the client to trust the project workspace before any run
        // proceeds. One-time, workspace-level (not per-call): a project not
        // yet acknowledged in user-level settings prompts once, persists the
        // answer on accept, and ends the session on decline. No-op for a
        // non-project session (project_path None) or an already-trusted path.
        let _trust = self.ensure_trust(&mut io).await?;
        // A reattaching connection may find a parked PendingTurn in the host
        // store (the prior connection disconnected mid-permission). Re-emit
        // the remaining asks + resume before entering the frame loop. No-op
        // on the single-shot path (host None) or when no turn is parked.
        session::resume_pending(&mut self, &mut io).await?;
        // Ship durable events that predate this connection: a resumed
        // session's seeded/loaded history (restore_trajectory backfilled the
        // mirror at composition), or a reattach's missed-while-disconnected
        // delta. pushed_count starts at 0 on a fresh server, so this replays
        // the full trajectory; resume_pending already advanced it for the
        // parked-turn case, making this a no-op there. The client's frame
        // stream carries the history so the working screen renders it, not
        // just the status bar.
        self.push_new_events(&mut io).await?;
        loop {
            let Some(frame) = io.next_frame().await else {
                // Clean client disconnect.
                self.flush_pushed_count();
                return Ok(());
            };
            // A session/* notification between runs: session/inject enqueues
            // a message for the next run's mid-turn drain (a submit that
            // landed as the prior run ended — the race the run-boundary queue
            // catches); session/queue_remove drops a queued message (overlay
            // delete, or popping the head to start a follow-up run).
            // session/cancel is a real no-op here: there is no active run, and
            // routing it to abort() would set the durable aborted flag (which
            // survives across the Interruption boundary) and silently short-
            // circuit the next resume — dropping a later approval with no
            // signal. So cancel is dropped between runs; inject/queue_remove
            // still go through handle_session_notification. A request frame
            // also parses as a notification (the id is ignored), so dispatch
            // by method name + fall through to ClientFrame for anything else
            // — otherwise a message/send would be swallowed.
            if let Ok(notif) =
                serde_json::from_str::<houyicoder_protocol::acp_wire::AcpNotification>(&frame)
            {
                match notif.method.as_str() {
                    "session/cancel" => continue,
                    "session/inject" | "session/queue_remove" => {
                        self.handle_session_notification(&notif);
                        continue;
                    }
                    _ => {}
                }
            }
            let client_frame: ClientFrame = match serde_json::from_str(&frame) {
                Ok(cf) => cf,
                Err(e) => {
                    self.send_wire_error(
                        &mut io,
                        WireError::new(WireErrorKind::InvalidFrame, e.to_string(), false),
                    )
                    .await?;
                    continue;
                }
            };
            let req = match client_frame {
                ClientFrame::Request(req) => req,
                // A bare reverse-response outside a run is unexpected: the
                // serve loop only reads between runs; mid-run responses are
                // consumed by handle_message_send. Fail closed rather than
                // dropping the frame silently.
                ClientFrame::Response(resp) => {
                    self.send_wire_error(
                        &mut io,
                        WireError::new(
                            WireErrorKind::InvalidFrame,
                            format!("unexpected reverse response for req_id {}", resp.req_id.0),
                            false,
                        ),
                    )
                    .await?;
                    continue;
                }
                // non_exhaustive guard: a future client-frame shape the server
                // does not know yet. Fail closed.
                _ => {
                    self.send_wire_error(
                        &mut io,
                        WireError::new(
                            WireErrorKind::InvalidFrame,
                            "unknown client frame shape",
                            false,
                        ),
                    )
                    .await?;
                    continue;
                }
            };
            if let Err(e) = self.dispatch(&mut io, req).await {
                // A carrier-level failure (client gone) is fatal to the loop;
                // a per-request wire error is sent inside dispatch and we
                // continue. Best-effort surface then return.
                self.send_wire_error(&mut io, e.clone()).await.ok();
                return Err(e);
            }
        }
    }

    /// Exchange Hello with the client. Both ends send their Hello first; the
    /// server validates the client's version and advertised capabilities. A
    /// version mismatch fails non-retriable so a peer never enters a
    /// half-working session.
    async fn handshake(&mut self, io: &mut ServerIo) -> Result<Negotiated, WireError> {
        let local = Hello::local();
        // Send our Hello first so a peer waiting on it can proceed; then read
        // the peer's Hello and validate it.
        self.send_typed(io, &local).await?;
        let Some(frame) = io.next_frame().await else {
            self.flush_pushed_count();
            return Err(WireError::new(
                WireErrorKind::Unavailable,
                "client closed before hello",
                false,
            ));
        };
        let peer: Hello = serde_json::from_str(&frame)
            .map_err(|e| WireError::new(WireErrorKind::InvalidFrame, e.to_string(), false))?;
        // A client-declared replay cursor overrides the holder-side pushed
        // count: a fresh client (count 0) gets the whole trajectory replayed,
        // a reconnecting client gets only what it missed. None means the
        // client does not track the count, so the holder-side cursor (the
        // prior connection's pushed count) stands.
        if let Some(count) = peer.last_event_count {
            self.pushed_count = count as usize;
        }
        negotiate(&local, &peer)
    }

    /// Drive one user message through the runner. The run streams its turn
    /// events into the session log; on completion the service forwards each
    /// event as a TurnEvent frame on the seq stream, then returns the run
    /// outcome as a response correlated to the request.
    ///
    /// Permission asks are surfaced mid-turn as reverse requests: when the
    /// engine returns RunOutcome::Interruption the service does NOT end the
    /// run — it emits one ServerRequestEnvelope::Permission per pending ask,
    /// reads back one ClientResponseEnvelope::Permission per ask, applies the
    /// decisions via runner.resume, and loops. Only when the run ends with a
    /// final outcome does the service send the RunOk response. The half-live
    /// turn state machine lives here (the composition root + protocol server),
    /// not in the runner and not in the wire.
    #[expect(clippy::too_many_lines, reason = "message dispatch")]
    async fn handle_message_send(
        &mut self,
        io: &mut ServerIo,
        req_id: RequestId,
        content: Vec<ContentBlock>,
    ) -> Result<(), WireError> {
        // Collapse the multimodal content to the plain text the engine run
        // takes today. Non-text blocks are dropped at the service boundary
        // until a multimodal run path lands; an empty content vec degenerates
        // to an empty input string.
        let text: String = content
            .into_iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text),
                // Non-text dropped at the service boundary until multimodal
                // run lands.
                _ => None,
            })
            .collect();
        let mut result = {
            // Clone the Arc so run_fut borrows the local clone, not self.runner
            // — this frees self for the select's notify branch to drain mid-run
            // (route B). Without it run_fut would hold &self.runner and the
            // notify branch could not take &mut self to push new events.
            let runner = Arc::clone(&self.runner);
            let run_fut = runner.run(self.session, text);
            tokio::pin!(run_fut);
            loop {
                // Either the shared store Notify (route B mid-run drain) or a
                // never-resolving pending future when no Notify is wired (None
                // => behavior is exactly today's: events push only at resolve).
                let notify_fut = match &self.append_notify {
                    Some(n) => futures::future::Either::Left(n.notified()),
                    None => futures::future::Either::Right(futures::future::pending::<()>()),
                };
                tokio::select! {
                    biased;
                    r = &mut run_fut => break r,
                    frame = io.next_frame() => match frame {
                        Some(f) => {
                            // A session/* notification during the streaming
                            // phase: session/cancel aborts the run;
                            // session/inject enqueues a message for mid-turn
                            // injection; session/queue_remove drops a queued
                            // message. Fire-and-forget — the effect shows up
                            // in the run outcome + transcript.
                            if let Ok(notif) = serde_json::from_str::<houyicoder_protocol::acp_wire::AcpNotification>(&f) {
                                self.handle_session_notification(&notif);
                            }
                            // Permission mode change mid-run: update the gate
                            // so the next tool call sees the new mode. The
                            // gate is Mutex-protected; the drive loop reads it
                            // at decide() time, so the switch is immediate.
                            // Child transcript fetch is read-only, safe mid-run.
                            else if let Ok(ClientFrame::Request(req)) = serde_json::from_str::<ClientFrame>(&f) {
                                self.handle_request_during_run(io, req).await;
                            }
                        }
                        None => {
                            self.flush_pushed_count();
                            return Err(WireError::new(
                                WireErrorKind::Unavailable,
                                "client closed mid-run",
                                false,
                            ));
                        }
                    },
                    _ = notify_fut => {
                        // A store append landed mid-run: drain the new durable
                        // events so a tool-call frame ships while the run is
                        // still in flight. push_new_events is the shared
                        // cursor drain the post-resolve outer loop also uses;
                        // spurious wakes (a background extract/dream fork on
                        // the same store) just re-run the cursor with nothing
                        // new to skip — no correctness impact, never assert
                        // "new events must exist" here.
                        self.push_new_events(io).await?;
                        tokio::task::yield_now().await;
                    },
                }
            }
        };
        loop {
            // Push only the events appended since the last push so a resumed
            // run does not re-send the events the client already saw. The
            // trajectory mirror is append-ordered and in-process, so the
            // count cursor skips exactly the already-pushed prefix; a run
            // that resumes after a permission ask lands only its new events
            // (the audit verdict + the post-resume tool results), not a
            // duplicate of the pre-ask stream.
            self.push_new_events(io).await?;
            // Flush the mid-turn queue's consumed texts so the frontend can
            // drop them from its mirror (a consumed message is no longer
            // pending). Sent at every outer-loop iteration — Interruption +
            // terminal — so the mirror reconciles incrementally, never
            // stranding a consumed item or double-spawning it at run end.
            let consumed = self.runner.take_consumed_input();
            if !consumed.is_empty() {
                self.send_event(io, FrontendEventKind::QueueConsumed { texts: consumed })
                    .await?;
            }
            match result {
                Ok(run) => match run.outcome {
                    houyicoder_core::agent::RunOutcome::Interruption(approvals) => {
                        // Half-live turn: send one reverse permission ask per
                        // pending approval, read one decision back per ask,
                        // then resume.
                        // Persist the whole turn before the first ask so a
                        // disconnect at any point in the batch retains the
                        // unanswered asks plus the verdicts already given.
                        // Single-shot (host None) skips this — no reconnect.
                        if let Some(host) = &self.host {
                            let remaining = approvals
                                .iter()
                                .map(|a| crate::lifecycle::PendingPermission {
                                    call_id: a.call_id.clone(),
                                    tool: a.tool_name.clone(),
                                    input: a.input.clone(),
                                })
                                .collect::<Vec<_>>();
                            host.store().set_pending(
                                self.session,
                                Some(crate::lifecycle::PendingTurn {
                                    remaining,
                                    decided: Vec::new(),
                                }),
                            );
                        }
                        let mut decisions = Vec::with_capacity(approvals.len());
                        for approval in approvals {
                            let decision = self.handle_approval(io, &approval).await?;
                            decisions.push(decision);
                            // A cancel mid-ask aborts the run; stop asking the
                            // remaining approvals so resume() (which checks the
                            // durable aborted flag) can surface the cancellation
                            // instead of hanging on a response the client will not
                            // send for approval #2.
                            if self.runner.is_aborted() {
                                break;
                            }
                        }
                        // All asks answered: clear the parked turn before
                        // resume so a reconnect mid-resume does not re-emit.
                        if let Some(host) = &self.host {
                            host.store().set_pending(self.session, None);
                        }
                        result = {
                            let resume_fut = self.runner.resume(self.session, &decisions);
                            tokio::pin!(resume_fut);
                            loop {
                                tokio::select! {
                                    biased;
                                    r = &mut resume_fut => break r,
                                    frame = io.next_frame() => match frame {
                                        Some(f) => {
                                            // session/* notification mid-resume:
                                            // same dispatch as mid-run (cancel /
                                            // inject / queue_remove). A
                                            // session/inject here enqueues for
                                            // the next turn boundary after the
                                            // resumed run re-enters the drive
                                            // loop.
                                            if let Ok(notif) = serde_json::from_str::<houyicoder_protocol::acp_wire::AcpNotification>(&f) {
                                                self.handle_session_notification(&notif);
                                            }
                                            // Permission mode change mid-resume:
                                            // same semantics as mid-run; child
                                            // transcript fetch also safe.
                                            else if let Ok(ClientFrame::Request(req)) = serde_json::from_str::<ClientFrame>(&f) {
                                                self.handle_request_during_run(io, req).await;
                                            }
                                        }
                                        None => {
                                            self.flush_pushed_count();
                                            return Err(WireError::new(
                                                WireErrorKind::Unavailable,
                                                "client closed mid-resume",
                                                false,
                                            ));
                                        }
                                    },
                                }
                            }
                        };
                        continue;
                    }
                    // Final outcome: the wire no longer carries an
                    // Interruption variant; the other arms are the final ones.
                    _ => {
                        let payload = ResponsePayload::RunOk(project_run_result(&run));
                        return self.send_response(io, req_id, payload).await;
                    }
                },
                Err(e) => {
                    let payload = ResponsePayload::RunErr(project_run_error(&e));
                    return self.send_response(io, req_id, payload).await;
                }
            }
        }
    }
}

/// The current wall clock as milliseconds since the Unix epoch. Used for the
/// ts field on a server-appended audit event; falls back to 0 if the clock is
/// before the epoch (not expected outside a misconfigured test harness).
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod git_ops_tests {
    use super::Server;
    use houyicoder_permission::ModeGate;
    use houyicoder_protocol::envelope::ResponsePayload;
    use std::sync::Arc;

    /// Build a Server over a stub runner + a real gate (returned so the test
    /// can inspect how a consent was recorded).
    fn server_with_gate() -> (Server, Arc<houyicoder_permission::DefaultModeGate>) {
        let store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        )));
        let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
            Arc::new(houyicoder_provider::FakeProvider::text("test"));
        let runner = houyicoder_core::agent::Runner::with_shared_store(
            store,
            provider,
            houyicoder_core::agent::ToolRegistry::new(),
            houyicoder_core::agent::runner_config::RunnerConfig::default(),
        );
        let gate = Arc::new(houyicoder_permission::DefaultModeGate::new());
        let server = Server::new(
            Arc::new(runner),
            houyicoder_context::SessionId::new(),
            gate.clone(),
        );
        (server, gate)
    }

    #[test]
    fn test_git_ops_query_set() {
        let (server, _gate) = server_with_gate();
        // Query: default on.
        assert!(matches!(
            server.ask_before_git_response(None),
            ResponsePayload::PermissionAskBeforeGit(true)
        ));
        // Set off; the reply carries the new state.
        assert!(matches!(
            server.ask_before_git_response(Some(false)),
            ResponsePayload::PermissionAskBeforeGit(false)
        ));
        // A follow-up query reflects the set.
        assert!(matches!(
            server.ask_before_git_response(None),
            ResponsePayload::PermissionAskBeforeGit(false)
        ));
    }

    #[test]
    fn test_git_consent_skips_rule() {
        let (server, gate) = server_with_gate();
        let durable = |g: &std::sync::Arc<houyicoder_permission::DefaultModeGate>| {
            g.rules().iter().filter(|r| r.scope.is_writable()).count()
        };
        // The gate seeds four builtin rules at construction; none are durable.
        assert_eq!(durable(&gate), 0, "no durable rules at start");
        // A git commit approval routes to a session-scope allow rule (in-memory,
        // not durable), shadowing the builtin ask rule this session.
        server.apply_consent_rule("bash", &serde_json::json!({"command": "git commit -m x"}));
        assert_eq!(
            durable(&gate),
            0,
            "git op consent stays session-scoped, no durable rule"
        );
        // A non-git command takes the durable-rule path (contrast).
        server.apply_consent_rule("bash", &serde_json::json!({"command": "npm install"}));
        assert_eq!(durable(&gate), 1, "non-git command becomes a durable rule");
    }
}

#[cfg(test)]
mod ask_rule_tests {
    use houyicoder_permission::{Effect, Rule, RuleContent};

    /// An Ask rule round-trips through the projection losslessly: engine
    /// Effect::Ask -> wire Ask -> engine Effect::Ask (was wrongly dropped to
    /// Reject before the three-state wire fix).
    #[test]
    fn test_ask_rule_round_trips() {
        let engine_rule =
            Rule::with_content("bash", RuleContent::Prefix("git push".into()), Effect::Ask)
                .unwrap();
        let wire = crate::projection::project_permission_rule(&engine_rule);
        assert_eq!(
            wire.effect,
            houyicoder_protocol::frontend::permission::PermissionEffect::Ask
        );
        let back = crate::projection::wire_rule_to_engine(&wire).unwrap();
        assert_eq!(
            back.effect,
            Effect::Ask,
            "Ask must round-trip, not drop to Deny"
        );
    }

    /// The wire Ask variant serializes as "ask" (snake_case).
    #[test]
    fn test_ask_effect_label() {
        use houyicoder_protocol::frontend::permission::PermissionEffect;
        assert_eq!(
            crate::projection::project_permission_rule(&Rule::new("edit", Effect::Ask).unwrap())
                .effect,
            PermissionEffect::Ask
        );
    }
}
