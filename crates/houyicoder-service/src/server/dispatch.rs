//! Request dispatch — routing one frontend request to its handler. Split
//! from server.rs as a child module so that file stays under the size gate;
//! same pattern as io / session / delta.

use houyicoder_protocol::envelope::{RequestEnvelope, ResponsePayload};
use houyicoder_protocol::wire::{WireError, WireErrorKind};

use super::{Server, io::ServerIo};

impl Server {
    /// Route one request to its handler. MessageSend drives the runner; RunCancel
    /// aborts the in-flight run; other verbs return Ack until their service-side
    /// handlers land. A handler returns Err only for a carrier-level failure
    /// (client gone); per-request invalidity is sent as a ResponsePayload::Error
    /// and returns Ok. The session_id a run verb carries must match this
    /// server's session; a mismatch fails closed (Error, not silent drop).
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    #[expect(clippy::cognitive_complexity, reason = "inherent dispatch")]
    pub(super) async fn dispatch(
        &mut self,
        io: &mut ServerIo,
        req: RequestEnvelope,
    ) -> Result<(), WireError> {
        let req_id = req.req_id;
        match req.payload {
            houyicoder_protocol::frontend::FrontendRequest::MessageSend {
                session_id,
                content,
            } => {
                if !self.session_matches(&session_id) {
                    return self
                        .send_response(
                            io,
                            req_id,
                            ResponsePayload::Error(WireError::new(
                                WireErrorKind::InvalidRequest,
                                format!("session id mismatch: got {session_id}"),
                                false,
                            )),
                        )
                        .await;
                }
                self.handle_message_send(io, req_id, content).await
            }
            houyicoder_protocol::frontend::FrontendRequest::RunCancel {
                session_id,
                reason: _,
            } => {
                if !self.session_matches(&session_id) {
                    return self
                        .send_response(
                            io,
                            req_id,
                            ResponsePayload::Error(WireError::new(
                                WireErrorKind::InvalidRequest,
                                format!("session id mismatch: got {session_id}"),
                                false,
                            )),
                        )
                        .await;
                }
                // Abort the run (cancels an in-flight stream loop via the
                // token; sets the durable aborted flag only when the run is
                // paused on an ask). When there is no active run (idle cancel),
                // abort is a no-op on the flag — it does not poison a later
                // resume. The outcome of an in-flight run returns as the
                // response to the original run request; this request acks so
                // the client's correlation completes.
                self.runner.abort();
                self.send_response(io, req_id, ResponsePayload::Ack).await
            }
            houyicoder_protocol::frontend::FrontendRequest::Status => {
                let wire = self.status_snapshot_wire();
                self.send_response(io, req_id, ResponsePayload::Status(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::RenameSession { session_id, name } => {
                if !self.session_matches(&session_id) {
                    return self
                        .send_response(
                            io,
                            req_id,
                            ResponsePayload::Error(WireError::new(
                                WireErrorKind::InvalidRequest,
                                format!("session id mismatch: got {session_id}"),
                                false,
                            )),
                        )
                        .await;
                }
                // The sidecar store is None on the single-shot test path; the
                // TUI's stub mode never sends this request, but fail closed
                // with a clear error if it does. Internal (not InvalidRequest):
                // the client request is fine, the server lacks the store.
                let Some(store) = self.meta_store.as_ref() else {
                    return self
                        .send_response(
                            io,
                            req_id,
                            ResponsePayload::Error(WireError::new(
                                WireErrorKind::Internal,
                                "rename: no session meta store wired (stub mode)",
                                false,
                            )),
                        )
                        .await;
                };
                // Empty/whitespace clears back to Auto so the display reverts
                // to the first-prompt slug; non-empty marks User so a later
                // auto-derivation does not clobber the custom name. Applied
                // through update_meta so a model switch landing at the same
                // moment does not write back the pre-rename name.
                let trimmed = name.trim().to_string();
                let outcome = store.update_meta(self.session, &mut |meta| {
                    if trimmed.is_empty() {
                        meta.name = None;
                        meta.name_source = houyicoder_context::NameSource::Auto;
                    } else {
                        meta.name = Some(trimmed.clone());
                        meta.name_source = houyicoder_context::NameSource::User;
                    }
                });
                let detail = match outcome {
                    Ok(houyicoder_context::MetaUpdate::Written) => None,
                    Ok(houyicoder_context::MetaUpdate::Absent) => {
                        Some("rename: no session sidecar to rename".to_string())
                    }
                    Err(e) => Some(format!("rename: write failed: {e}")),
                };
                if let Some(detail) = detail {
                    return self
                        .send_response(
                            io,
                            req_id,
                            ResponsePayload::Error(WireError::new(
                                WireErrorKind::Internal,
                                detail,
                                false,
                            )),
                        )
                        .await;
                }
                // Reply with a fresh status snapshot so the host re-renders
                // /status + the picker reflects the new name on the next list.
                let wire = self.status_snapshot_wire();
                self.send_response(io, req_id, ResponsePayload::Status(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::ToolList => {
                let wire =
                    self.runner
                        .tools_snapshot()
                        .into_iter()
                        .map(|(name, description)| {
                            houyicoder_protocol::frontend::tools::ToolEntry { name, description }
                        })
                        .collect::<Vec<_>>();
                self.send_response(io, req_id, ResponsePayload::Tools(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::Agents => {
                // The directory doubles as the model's prompt paragraph
                // (byte-stable for the prompt cache); the panel renders it
                // verbatim.
                let dir = self.runner.agent_directory().unwrap_or_default();
                self.send_response(io, req_id, ResponsePayload::Agents(dir))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::ChildTranscript { child_sid } => {
                let frames = self.child_transcript_frames(&child_sid).await;
                self.send_response(
                    io,
                    req_id,
                    ResponsePayload::ChildTranscript { child_sid, frames },
                )
                .await
            }
            houyicoder_protocol::frontend::FrontendRequest::Hooks => {
                // The full hook event surface: the framework's declared events
                // (with live-fire markers) plus the registered external hooks.
                // The user sees what the hook system CAN do, not just what a
                // config happened to register.
                let mut wire = hooks_to_wire(self.runner.hooks_list());
                wire.extend(hook_events_to_wire());
                wire.sort_by(|a, b| a.name.cmp(&b.name));
                self.send_response(io, req_id, ResponsePayload::Hooks(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::Undo => {
                let desc = match self.runner.undo_last() {
                    houyicoder_core::snapshot::UndoOutcome::Restored(entry) => {
                        Some(entry.description())
                    }
                    houyicoder_core::snapshot::UndoOutcome::Empty => None,
                    houyicoder_core::snapshot::UndoOutcome::Failed(msg) => {
                        Some(format!("restore failed: {msg}"))
                    }
                };
                self.send_response(io, req_id, ResponsePayload::UndoResult(desc))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::ModelInfo => {
                self.handle_model_info(io, req_id).await
            }
            houyicoder_protocol::frontend::FrontendRequest::ModelSet {
                model,
                effort,
                effort_toggled,
            } => {
                // Resolve the Default sentinel, swap the model, set the session
                // effort, persist the pick (settings + sidecar), and reply
                // with the actually-applied model + effort. Best-effort
                // persistence: a write failure does not fail the request.
                let resolved = model
                    .as_deref()
                    .map(str::to_string)
                    .unwrap_or_else(houyicoder_config::resolve_model);
                self.runner.set_model(resolved.clone());
                self.runner.set_effort(effort);
                // Best-effort: a write failure does not fail the request —
                // the in-memory pick still takes effect this session.
                crate::composition::persist_model_pick(
                    &self.settings_path,
                    model.as_deref(),
                    effort,
                    effort_toggled,
                );
                self.persist_sidecar_model(&resolved);
                self.send_response(
                    io,
                    req_id,
                    ResponsePayload::ModelResult(houyicoder_protocol::envelope::ModelApplied {
                        model: self.runner.active_model(),
                        effort: self.runner.resolve_applied_effort(),
                    }),
                )
                .await
            }
            houyicoder_protocol::frontend::FrontendRequest::Trajectory => {
                let events = self.runner.store().trajectory_snapshot(self.session);
                let entries = crate::projection::project_trajectory(&events);
                let redundant = crate::projection::redundant::project_redundant(
                    &self.runner.redundancy_snapshot(),
                );
                let unknown_count = events
                    .iter()
                    .filter(|e| matches!(e.kind, houyicoder_context::TurnEventKind::Unknown))
                    .count() as u32;
                let wire = houyicoder_protocol::frontend::trajectory::TrajectoryResponse {
                    entries,
                    redundant,
                    unknown_count,
                };
                self.send_response(io, req_id, ResponsePayload::Trajectory(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::Context => {
                let snap = self.runner.status_snapshot();
                // When no turn has run yet (context_served() is None), build a
                // prospective view — what the model would see on the first turn
                // (system prompt + tools, messages = 0) — so /context is never
                // empty on a fresh session. Computed synchronously at
                // command time.
                let served = self
                    .runner
                    .context_served()
                    .unwrap_or_else(|| self.runner.context_prospective());
                let mut bd = served.breakdown(&snap.model, snap.context_window);
                bd.compact_summary = self.runner.compact_summary(self.session).await;
                // Compact buffer: summary text token count, injected as a
                // category so the /context grid shows it separately.
                let summary_tokens = self.runner.compact_summary_tokens(self.session).await;
                if summary_tokens > 0 {
                    let insert_at = bd
                        .categories
                        .iter()
                        .position(|c| c.label == "Free space")
                        .unwrap_or(bd.categories.len());
                    bd.categories.insert(
                        insert_at,
                        houyicoder_core::agent::CategoryBreakdown {
                            label: "Compact buffer".into(),
                            color_hint: 61,
                            tokens: summary_tokens,
                            is_deferred: false,
                            is_reserved: false,
                        },
                    );
                }
                // Cache prefix = System prompt + Tools section tokens.
                let prefix: u32 = served
                    .section(houyicoder_core::agent::SectionKind::SystemPrompt)
                    .map(|s| s.tokens)
                    .unwrap_or(0)
                    + served
                        .section(houyicoder_core::agent::SectionKind::Tools)
                        .map(|s| s.tokens)
                        .unwrap_or(0);
                bd.cache_prefix_tokens = Some(prefix);
                // Hit rate = cache_read / input_tokens from cumulative usage.
                let usage = &snap.cumulative_usage;
                if usage.input_tokens > 0 {
                    bd.cache_hit_rate =
                        Some(usage.cache_read_input_tokens as f64 / usage.input_tokens as f64);
                }
                let wire = crate::projection::project_context_breakdown(&bd);
                self.send_response(io, req_id, ResponsePayload::Context(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::Compact => {
                // Manual /compact: fire PreCompact hooks, fold older events
                // into a summary, persist a CheckpointManifest, fire
                // PostCompact, then reply with the outcome. The runner fires
                // the hooks + runs marker extraction internally so the manual
                // path and the auto overflow path share one sequence. The
                // served view picks up the manifest on the next turn —
                // compaction does not reduce the in-flight context
                // immediately, only the next served window. An error surfaces
                // as a ResponsePayload::Error so the host renders it rather
                // than hanging on the req_id.
                match self.runner.compact(self.session).await {
                    Ok(outcome) => {
                        let wire = crate::projection::compact::project_compact_reply(&outcome);
                        self.send_response(io, req_id, ResponsePayload::Compact(wire))
                            .await
                    }
                    Err(e) => {
                        self.send_response(
                            io,
                            req_id,
                            ResponsePayload::Error(WireError::new(
                                WireErrorKind::InvalidRequest,
                                e.to_string(),
                                false,
                            )),
                        )
                        .await
                    }
                }
            }
            houyicoder_protocol::frontend::FrontendRequest::MemoryList => {
                let wire =
                    crate::projection::memory::project_memory_list(self.runner.memory_list());
                self.send_response(io, req_id, ResponsePayload::MemoryList(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::MemoryShow { key } => {
                let wire =
                    crate::projection::memory::project_memory_entry(self.runner.memory_show(&key));
                self.send_response(io, req_id, ResponsePayload::MemoryShow(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::MemoryForget { key, scope } => {
                // NotFound is benign (the row is already gone); re-list. A
                // real failure (Io, bad path, corrupt, atomicity) must
                // surface to the user — otherwise the pane re-lists with the
                // entry still present and no indication the delete failed, so
                // the user believes the forget worked when it did not.
                match self.runner.memory_forget(&key, &scope) {
                    Ok(()) | Err(houyicoder_context::MemoryError::NotFound) => {
                        let wire = crate::projection::memory::project_memory_list(
                            self.runner.memory_list(),
                        );
                        self.send_response(io, req_id, ResponsePayload::MemoryList(wire))
                            .await
                    }
                    Err(e) => {
                        self.send_response(
                            io,
                            req_id,
                            ResponsePayload::Error(WireError::new(
                                WireErrorKind::Internal,
                                format!("memory forget failed: {e}"),
                                false,
                            )),
                        )
                        .await
                    }
                }
            }
            houyicoder_protocol::frontend::FrontendRequest::MemoryToggleState => {
                let (auto_memory, auto_dream) = self.runner.toggles_state();
                let wire = crate::projection::memory::project_toggle_state(auto_memory, auto_dream);
                self.send_response(io, req_id, ResponsePayload::ToggleState(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::MemoryToggle { which } => {
                use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
                let (auto_memory, auto_dream) = match which {
                    MemoryToggleWhich::Auto => {
                        let am = self.runner.flip_auto_memory();
                        let ad = self.runner.toggles_state().1;
                        (am, ad)
                    }
                    MemoryToggleWhich::Dream => {
                        let ad = self.runner.flip_auto_dream();
                        let am = self.runner.toggles_state().0;
                        (am, ad)
                    }
                };
                // Persist the new pair so the choice survives a restart. The
                // in-memory atomic already flipped, so a write failure only loses
                // cross-session persistence (the session still sees the flip).
                houyicoder_config::save_toggles_to(
                    &self.settings_path,
                    &houyicoder_config::MemoryToggles {
                        auto_memory,
                        auto_dream,
                    },
                );
                let wire = crate::projection::memory::project_toggle_state(auto_memory, auto_dream);
                self.send_response(io, req_id, ResponsePayload::ToggleState(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::PermissionMode => {
                let wire = crate::projection::project_permission_mode(self.gate.current());
                self.send_response(io, req_id, ResponsePayload::PermissionMode(wire))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::PermissionRules => {
                self.send_response(
                    io,
                    req_id,
                    ResponsePayload::PermissionRules(self.rule_set_wire()),
                )
                .await
            }
            houyicoder_protocol::frontend::FrontendRequest::PermissionCycleMode => {
                let resp = match self.gate.tab_cycle() {
                    Ok(mode) => ResponsePayload::PermissionMode(
                        crate::projection::project_permission_mode(mode),
                    ),
                    Err(e) => ResponsePayload::Error(WireError::new(
                        WireErrorKind::InvalidRequest,
                        e.to_string(),
                        false,
                    )),
                };
                self.send_response(io, req_id, resp).await
            }
            houyicoder_protocol::frontend::FrontendRequest::PermissionAddRule { rule } => {
                let resp = match crate::projection::wire_rule_to_engine(&rule) {
                    Ok(r) => {
                        self.gate.add_rule(r);
                        ResponsePayload::PermissionRules(self.rule_set_wire())
                    }
                    Err(e) => ResponsePayload::Error(WireError::new(
                        WireErrorKind::InvalidRequest,
                        e.to_string(),
                        false,
                    )),
                };
                self.send_response(io, req_id, resp).await
            }
            houyicoder_protocol::frontend::FrontendRequest::PermissionRemoveRule { index } => {
                // Respond with the updated rule set (mirrors AddRule) so the
                // frontend's rules_cache stays in sync after a delete.
                let resp = if self.gate.remove_rule(index) {
                    ResponsePayload::PermissionRules(self.rule_set_wire())
                } else {
                    ResponsePayload::Error(WireError::new(
                        WireErrorKind::InvalidRequest,
                        "rule index out of range",
                        false,
                    ))
                };
                self.send_response(io, req_id, resp).await
            }
            houyicoder_protocol::frontend::FrontendRequest::PermissionAddWorkingDir { path } => {
                // Extend the kernel fence (the seatbelt allow-back) so the
                // agent's next exec can touch the directory. The session
                // canonicalizes + validates it is a directory; a bad path
                // surfaces as an Error. Respond with the updated list so the
                // Workspace tab stays in sync without a poll.
                let resp = match &self.sandbox_session {
                    Some(s) => match s.add_working_dir(&path) {
                        Ok(()) => ResponsePayload::PermissionWorkingDirs(self.working_dirs_wire()),
                        Err(e) => ResponsePayload::Error(WireError::new(
                            WireErrorKind::InvalidRequest,
                            e.to_string(),
                            false,
                        )),
                    },
                    None => ResponsePayload::Error(WireError::new(
                        WireErrorKind::InvalidRequest,
                        "no sandbox session attached; working dirs need a runtime-mutable fence",
                        false,
                    )),
                };
                self.send_response(io, req_id, resp).await
            }
            houyicoder_protocol::frontend::FrontendRequest::PermissionRemoveWorkingDir { path } => {
                // No-op when the path was never added; respond with the
                // updated list either way (mirrors AddWorkingDir).
                if let Some(s) = &self.sandbox_session {
                    s.remove_working_dir(&path);
                }
                self.send_response(
                    io,
                    req_id,
                    ResponsePayload::PermissionWorkingDirs(self.working_dirs_wire()),
                )
                .await
            }
            houyicoder_protocol::frontend::FrontendRequest::PermissionAskBeforeGit { enabled } => {
                // None queries; Some sets. Always reply with the resulting
                // state so the /permission view stays in sync.
                self.send_response(io, req_id, self.ask_before_git_response(enabled))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::DebugSet { level } => {
                self.send_response(io, req_id, self.debug_response(level))
                    .await
            }
            houyicoder_protocol::frontend::FrontendRequest::SessionReset { session_id } => {
                if session_id.0 != self.session.to_string() {
                    return self
                        .send_response(
                            io,
                            req_id,
                            ResponsePayload::Error(WireError::new(
                                WireErrorKind::InvalidRequest,
                                "session id mismatch",
                                false,
                            )),
                        )
                        .await;
                }
                // Before-clear marker extraction: save unsolved-problem +
                // key-decision markers to the auto scope so key facts survive
                // the /clear drop (the "do not lose" invariant). Best-effort:
                // a failure logs and the clear still proceeds.
                if let Err(e) = self.runner.before_clear(self.session).await {
                    tracing::warn!("before-clear extraction failed: {e}");
                }
                self.runner.reset_usage();
                self.runner.reset_trajectory(self.session);
                // A state-changing command invalidates the server's
                // injection buffer: a message the host queued before this
                // reset was authored for the pre-clear context + must not
                // survive into the cleared one. The host is the single truth
                // source; the server queue is the current run's buffer only.
                self.runner.clear_input_queue();
                // The trajectory cursor tracks how many events this client has
                // seen; a reset clears the log, so the cursor rewinds to zero
                // (the next run replays from a fresh log).
                self.pushed_count = 0;
                self.send_response(io, req_id, ResponsePayload::Ack).await
            }
            // Unsupported verb for the current handler set; acknowledge so the
            // client's response correlation completes without hanging.
            _ => self.send_response(io, req_id, ResponsePayload::Ack).await,
        }
    }

    /// True when the wire session id names this server's session. The wire id
    /// is a display string; the engine session is ULID-backed, so the match is
    /// on the display form.
    pub(super) fn session_matches(
        &self,
        wire_id: &houyicoder_protocol::frontend::SessionId,
    ) -> bool {
        wire_id.0 == self.session.to_string()
    }

    /// Project the gate's durable rule set to the wire form for /rules
    /// replies + the add/remove acks that ship the updated set.
    fn rule_set_wire(&self) -> Vec<houyicoder_protocol::frontend::permission::PermissionRule> {
        // The /permissions management view lists durable (writable-scope)
        // rules only — builtin rules ship with the binary and session rules
        // are transient in-memory consent, neither is user-managed here;
        // individual seeded rules cannot be disabled yet (no separate
        // builtin section).
        self.gate
            .rules()
            .iter()
            .filter(|r| r.scope.is_writable())
            .map(crate::projection::project_permission_rule)
            .collect()
    }

    /// Project the sandbox session's runtime working dirs for the Workspace
    /// tab + the add/remove acks. Empty when no session is attached.
    fn working_dirs_wire(&self) -> Vec<String> {
        self.sandbox_session
            .as_ref()
            .map(|s| s.working_dirs())
            .unwrap_or_default()
    }

    /// Build a wire StatusSnapshot for the current session, attaching the
    /// sidecar identity fields (version / name / cwd / provenance) with
    /// auto-derivation of an unnamed session's name from the first prompt in
    /// the log head. Shared by the Status request + the RenameSession reply
    /// so both project the same post-rename view. The store is None on the
    /// single-shot test path; the snapshot degrades to runner-only fields.
    fn status_snapshot_wire(&self) -> houyicoder_protocol::frontend::status::StatusSnapshot {
        let snap = self.runner.status_snapshot();
        let mut wire = crate::projection::project_status(&snap);
        if let Some(store) = self.meta_store.as_ref()
            && let Some(mut meta) = store.read_meta(self.session)
        {
            if meta
                .name
                .as_ref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            {
                meta.name = first_prompt_slug(self.runner.store().as_ref(), self.session);
            }
            wire.meta = Some(crate::projection::project_session_meta(&meta));
        }
        // Running build version; always known, not sidecar-gated.
        wire.version = env!("CARGO_PKG_VERSION").to_string();
        // Attach the env-config display fields (auth token source, base URL,
        // setting sources) so the TUI renders them without importing the
        // config crate. The token source is the env var NAME, never the
        // secret value.
        wire.auth_token_source = super::status_wire::auth_token_source();
        wire.base_url = houyicoder_config::resolve_base_url();
        wire.setting_sources = super::status_wire::setting_sources_label();
        let (toggles, _settings_warnings) = houyicoder_config::load_toggles();
        wire.auto_memory = toggles.auto_memory;
        wire.auto_dream = toggles.auto_dream;
        wire.by_model = super::status_wire::project_by_model(self.runner.by_model_usage());
        wire
    }
}

/// Convert core HookEntry list to wire HookEntry list (the /hooks server
/// response). Pure so it is unit-testable without a Server or Runner. A
/// registered hook is fired iff any of its events has a live dispatch point.
pub(crate) fn hooks_to_wire(
    entries: Vec<houyicoder_core::agent::HookEntry>,
) -> Vec<houyicoder_protocol::frontend::hooks::HookEntry> {
    entries
        .into_iter()
        .map(|h| houyicoder_protocol::frontend::hooks::HookEntry {
            name: h.name,
            events: h.events.iter().map(|e| format!("{e:?}")).collect(),
            source: format!("{:?}", h.source),
            fired: h.events.iter().any(|e| e.is_fired()),
            summary: String::new(),
        })
        .collect()
}

/// The framework's declared hook-event surface, each as a wire entry (name +
/// live-fire marker). Source is "framework"; fired marks the three events with
/// a live dispatch point. Lets /hooks show what the hook system can do even
/// when no external hooks are configured.
pub(crate) fn hook_events_to_wire() -> Vec<houyicoder_protocol::frontend::hooks::HookEntry> {
    houyicoder_core::agent::HookEvent::ALL
        .iter()
        .copied()
        .map(|e| houyicoder_protocol::frontend::hooks::HookEntry {
            name: e.label().to_string(),
            events: vec![e.label().to_string()],
            source: "framework".to_string(),
            fired: e.is_fired(),
            summary: e.summary().to_string(),
        })
        .collect()
}

#[cfg(test)]
#[path = "dispatch_hook_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "trajectory_handler_tests.rs"]
mod trajectory_handler_tests;

/// Auto-derive a session name from the first user prompt in the log head, so
/// an unnamed session (name_source=Auto) shows a slug instead of blank. Reads
/// only the bounded log head, not a full replay, so it stays cheap even for a
/// large resumed session.
fn first_prompt_slug(
    log: &dyn houyicoder_api::session::SessionLog,
    session: houyicoder_context::SessionId,
) -> Option<String> {
    use houyicoder_context::{TurnEvent, TurnEventKind};
    let read = log.backend().read_log_range(session, 0, 64_000);
    for (_, line) in &read.lines {
        if let Ok(ev) = serde_json::from_str::<TurnEvent>(line)
            && let TurnEventKind::UserInput { text } = &ev.kind
        {
            return Some(slugify(text));
        }
    }
    None
}

/// Kebab-slugify a prompt to a compact session-name title (lowercase, dashes,
/// truncated). Mirrors the session-lister title derivation so /status and the
/// resume picker name the same session the same way.
fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = true;
    for c in text.chars().take(50) {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.chars().count() > 40 {
        out.chars().take(40).collect()
    } else {
        out
    }
}

#[cfg(test)]
#[path = "rename_session_tests.rs"]
mod rename_session_tests;

#[cfg(test)]
#[path = "memory_forget_dispatch_tests.rs"]
mod memory_forget_dispatch_tests;

#[cfg(test)]
#[path = "context_dispatch_tests.rs"]
mod context_dispatch_tests;

#[cfg(test)]
#[path = "model_set_tests.rs"]
mod model_set_tests;
