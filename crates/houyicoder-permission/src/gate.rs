//! The mode gate: the single entry point for mode state and the permission
//! decision. The gate holds the current mode, the rule set, the switch
//! history, the consent store, and the validator registry. It snapshots that
//! state into a shared context once per decision and runs the registry
//! lock-free against the snapshot, then applies the post-ladder transform so
//! no early return inside the ladder can skip the headless fallback.
//!
//! The per-mode policies (Manual / Auto) live here too: they are the
//! mode-default verdict the registry's final validator delegates to when
//! nothing else decided. They are pure functions of the request and its
//! side-effect level — no I/O, no mutable state, no classifier — so the
//! autonomous loop's relaxation of Exec and Filesystem is isolated and
//! testable apart from the gate's state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::compound::split_compound;
use crate::consent::ConsentStore;
use crate::decision::{AllowReason, AskReason, AskSource, Decision};
use crate::mode::{ModeChange, ModeError, PermissionMode, ToolRequest};
use crate::pipeline::{GateCtx, Pipeline};
use crate::rule::{Rule, evaluate, input_content};
use crate::side_effect::side_effect_for;
use houyicoder_api::sandbox::Containment;
use houyicoder_api::sandbox::SideEffect;

/// The single entry point for mode state. Implementors hold the current mode,
/// the rule set, and the switch history. GuardedTool holds an Arc<dyn ModeGate>
/// and consults decide; the TUI holds the concrete gate to manage rules and
/// read history.
pub trait ModeGate: Send + Sync {
    fn decide(&self, req: &ToolRequest) -> Decision;
    fn current(&self) -> PermissionMode;
    fn set_mode(&self, new: PermissionMode, reason: &str);
    fn tab_cycle(&self) -> Result<PermissionMode, ModeError>;
    /// The durable rule set. Used by the wire server to project /rules.
    fn rules(&self) -> Vec<Rule>;
    /// Add a durable rule (AllowAlways persists when a store is attached).
    fn add_rule(&self, rule: Rule);
    /// Persist a directory authorization (a path-bounds approval the user
    /// marked "always") to the durable store at the given scope, so the fence
    /// rehydrates it on restart. Default no-op (no store / a test gate). The
    /// fence's additional_dirs is updated separately by the caller.
    fn add_directory(&self, _dir: &std::path::Path, _scope: crate::store::Scope) {}
    /// Remove a rule by index. Returns true if the index was in range.
    fn remove_rule(&self, index: usize) -> bool;
    /// Enable or disable the git-checkpoint builtin rules (git commit / rebase
    /// / reset / tag ask). When disabled, the four builtin rules are dropped
    /// from the rule set so those ops fall through to the rest of the ladder
    /// (a plain git commit in Auto then Allows). The wire toggle maps here.
    fn set_git_checkpoint_enabled(&self, enabled: bool);
    /// Whether the git-checkpoint builtin rules are currently enabled.
    fn git_checkpoint_enabled(&self) -> bool;
    /// A snapshot of the in-process decision counter, bucketed by (outcome,
    /// source, validator), for the inspect / status surface and the harvest
    /// sprint's prompt-rate baseline. The default returns empty for gates
    /// that do not count.
    fn decision_metrics(&self) -> Vec<crate::metrics::DecisionBucket> {
        Vec::new()
    }
}

/// The default gate: a mode, a rule set, a switch history, and an optional
/// consent store. All mutable state behind a Mutex so Arc<dyn ModeGate> is Sync
/// and safe to share across the runner task and the TUI thread.
pub struct DefaultModeGate {
    mode: Mutex<PermissionMode>,
    rules: Mutex<Vec<Rule>>,
    history: Mutex<Vec<ModeChange>>,
    consent: Option<Arc<dyn crate::consent::ConsentStore>>,
    store: Option<Arc<dyn crate::store::RuleStore>>,
    /// The ordered validator registry. Built once at construction and never
    /// mutated, so the gate can share it across decide calls without a lock.
    /// The validators are stateless: every per-call input they need is packed
    /// into the shared context at the top of each decision.
    pipeline: Pipeline,
    /// Whether the session is non-interactive (session-host / CI). When true,
    /// any Ask decision degrades to Deny with a reason + a log line — there is
    /// no human to answer the prompt.
    headless: AtomicBool,
    /// The fence query interface. When set + auto_allow_fenced_exec
    /// is true, a fenced exec command skips the Ask and produces
    /// Allow(Containment(FenceProof)) instead. The fence is the authority;
    /// the gate does not second-guess it.
    containment: Option<Arc<dyn Containment>>,
    /// When true (the default), a fenced exec command is auto-allowed: the
    /// fence blocks at execution time if the call is not permitted, so
    /// asking the user is noise. When false, the gate asks for every exec
    /// command even when fenced (the conservative baseline).
    auto_allow_fenced_exec: AtomicBool,
    /// The in-process decision counter, bucketed by (outcome, source,
    /// validator). Feeds the harvest sprint's prompt-rate baseline and the
    /// inspect / status surface; no external metrics backend.
    metrics: crate::metrics::DecisionCounter,
    /// The ids of builtin rules the user disabled (e.g. via the /permission
    /// git toggle). A disabled builtin rule is not seeded into the rule set,
    /// so the call it gated falls through to the rest of the ladder.
    disabled_builtins: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl DefaultModeGate {
    pub fn new() -> Self {
        Self {
            mode: Mutex::new(PermissionMode::Auto),
            rules: Mutex::new(crate::rule::builtin_rules()),
            history: Mutex::new(Vec::new()),
            consent: None,
            store: None,
            pipeline: Pipeline::standard(),
            headless: AtomicBool::new(false),
            containment: None,
            auto_allow_fenced_exec: AtomicBool::new(false),
            metrics: crate::metrics::DecisionCounter::new(),
            disabled_builtins: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub fn with_mode(mode: PermissionMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            rules: Mutex::new(crate::rule::builtin_rules()),
            history: Mutex::new(Vec::new()),
            consent: None,
            store: None,
            pipeline: Pipeline::standard(),
            headless: AtomicBool::new(false),
            containment: None,
            auto_allow_fenced_exec: AtomicBool::new(false),
            metrics: crate::metrics::DecisionCounter::new(),
            disabled_builtins: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Test fixture: a gate with no builtin rules seeded, so store / persistence
    /// tests that count rules and reason about indices see only the rules they
    /// add. Production code uses new / with_mode, which seed the builtin set.
    #[cfg(test)]
    pub(crate) fn new_without_builtins() -> Self {
        Self {
            mode: Mutex::new(PermissionMode::Auto),
            rules: Mutex::new(Vec::new()),
            history: Mutex::new(Vec::new()),
            consent: None,
            store: None,
            pipeline: Pipeline::standard(),
            headless: AtomicBool::new(false),
            containment: None,
            auto_allow_fenced_exec: AtomicBool::new(false),
            metrics: crate::metrics::DecisionCounter::new(),
            disabled_builtins: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Attach a consent store. When the gate would otherwise Ask (either by
    /// rule or by default), it first consults the store; a hit within TTL
    /// upgrades the decision to Allow. Bypass-immune safety asks are not
    /// overridable by consent.
    pub fn with_consent(mut self, store: Arc<dyn ConsentStore>) -> Self {
        self.consent = Some(store);
        self
    }

    /// Attach a rule store and hydrate from it. The persisted rules become
    /// the initial in-memory set, so a new process that never held them
    /// starts with the union of user, project, and local scope rules.
    /// Subsequent add_rule calls persist every effect (allow/deny/ask) to
    /// the rule's chosen scope; remove_rule deletes from that scope too, so
    /// a deleted rule does not re-hydrate on restart.
    pub fn with_store(mut self, store: Arc<dyn crate::store::RuleStore>) -> Self {
        let persisted = store.load();
        if !persisted.is_empty() {
            self.rules
                .lock()
                .expect("mode gate mutex")
                .extend(persisted);
        }
        self.store = Some(store);
        self
    }

    /// Set whether the session is non-interactive (headless / CI). When true,
    /// any Ask decision degrades to Deny with a reason — there is no human to
    /// answer the prompt.
    pub fn with_headless(self, headless: bool) -> Self {
        self.headless.store(headless, Ordering::Relaxed);
        self
    }

    /// Attach the fence query interface. When set, the gate can build a
    /// FenceProof and auto-allow fenced exec commands (if
    /// auto_allow_fenced_exec is on).
    pub fn with_containment(mut self, c: Arc<dyn Containment>) -> Self {
        self.containment = Some(c.clone());
        // Rebuild the ladder so the path-bounds validator sees the fence. The
        // validator holds the handle directly (GateCtx stays fence-free by
        // design), so the pipeline must be rebuilt when containment attaches.
        self.pipeline = Pipeline::with_containment(c);
        self
    }

    /// Set whether fenced exec commands skip the Ask. Default off. Fence
    /// coverage only answers that the action stays in-bounds; it is not
    /// evidence the action is recoverable, so it cannot carry a silent
    /// auto-allow on its own. The relaxation stays off until a real
    /// recoverability proof (snapshot coverage of the call's targets) is
    /// available; tests that exercise the mechanism set it explicitly.
    pub fn with_auto_allow_fenced_exec(self, on: bool) -> Self {
        self.auto_allow_fenced_exec.store(on, Ordering::Relaxed);
        self
    }

    /// Post-construction setter for headless — the session-host path calls
    /// this when it knows no interactive frontend will answer prompts.
    pub fn set_headless(&self, headless: bool) {
        self.headless.store(headless, Ordering::Relaxed);
    }

    pub fn consent(&self) -> Option<Arc<dyn ConsentStore>> {
        self.consent.clone()
    }

    pub fn history(&self) -> Vec<ModeChange> {
        self.history.lock().expect("mode gate mutex").clone()
    }

    /// The raw gate pipeline without headless post-processing. The trait
    /// decide() wraps this to convert Ask → Deny when headless. The gate
    /// snapshots its mutable state into the shared context once — the rule
    /// set, the session consent set, the mode, and the transitional toggles —
    /// then runs the validator registry lock-free against that snapshot.
    fn decide_inner(&self, req: &ToolRequest) -> Decision {
        let rules = self.rules.lock().expect("mode gate mutex");
        let content = input_content(req.tool_name, req.input);
        let effect = evaluate(&rules, req.tool_name, &content);
        let segments = split_compound(&content);
        let mode = *self.mode.lock().expect("mode gate mutex");
        let ctx = GateCtx {
            mode,
            content: &content,
            segments: &segments,
            rules: &rules,
            consent: self.consent.as_deref(),
            effect,
            git_checkpoint_enabled: self.git_checkpoint_enabled(),
        };
        self.pipeline.decide(req, &ctx)
    }
}

impl Default for DefaultModeGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ModeGate for DefaultModeGate {
    fn decide(&self, req: &ToolRequest) -> Decision {
        // A debug-level span: every tool decision passes through here, so an
        // info-level span would emit one line per call and drown the quieter
        // events a diagnostic session is looking for. debug lets it surface
        // only when the operator raises the level to debug or below.
        let span = tracing::debug_span!(
            "permission_decide",
            outcome = tracing::field::Empty,
            source = tracing::field::Empty,
            validator = tracing::field::Empty,
        );
        let _enter = span.enter();
        let raw = self.decide_inner(req);
        let auto = self.auto_allow_fenced_exec.load(Ordering::Relaxed);
        let decision = crate::pipeline::post_transform::post_transform(
            raw,
            self.headless.load(Ordering::Relaxed),
            self.containment.as_deref(),
            auto,
            req,
        );
        span.record("outcome", crate::metrics::outcome_label(&decision));
        let labels = crate::metrics::decision_labels(&decision);
        span.record("source", labels.source);
        span.record("validator", labels.validator);
        self.metrics.inc(&decision);
        decision
    }

    fn current(&self) -> PermissionMode {
        *self.mode.lock().expect("mode gate mutex")
    }

    fn set_mode(&self, new: PermissionMode, reason: &str) {
        let mut mode = self.mode.lock().expect("mode gate mutex");
        let from = *mode;
        if from == new {
            return;
        }
        *mode = new;
        drop(mode);
        self.history
            .lock()
            .expect("mode gate mutex")
            .push(ModeChange {
                from,
                to: new,
                reason: reason.into(),
            });
    }

    fn tab_cycle(&self) -> Result<PermissionMode, ModeError> {
        let cur = self.current();
        match cur.tab_next() {
            Some(next) => {
                self.set_mode(next, "shift+tab cycle");
                Ok(next)
            }
            None => Err(ModeError(format!(
                "shift+tab does not cycle from {}",
                cur.label()
            ))),
        }
    }

    fn rules(&self) -> Vec<Rule> {
        self.rules.lock().expect("mode gate mutex").clone()
    }

    fn add_rule(&self, rule: Rule) {
        // Persist every durable rule (allow/deny/ask) to its chosen scope —
        // a rule is an always-X directive, not a one-time verdict, so all
        // effects survive restart. The destination the user picked is honored
        // for every effect (no silent session-only drop for deny/ask). Log
        // (don't silently drop) a store failure: the in-memory rule is added
        // but the disk write failed, so it will NOT survive restart — the
        // signal (an eprintln here) is a first step; TODO route the failure
        // back through the server as a ResponsePayload::Error so the TUI can
        // surface it (eprintln is invisible under the TUI's raw-mode alt
        // screen, but it at least leaves a trace in the launching shell).
        // Builtin-scoped rules are seeded at construction, not writable, so a
        // caller adding one never persists it (only the in-memory push).
        if rule.scope.is_writable()
            && let Some(store) = &self.store
            && let Err(e) = store.add(&rule)
        {
            tracing::warn!(
                "[permission] add_rule: in-memory rule added but the \
                 persistence write failed; it will not survive restart: {e}"
            );
        }
        // Dedup-push: the in-memory list is the single source of truth for
        // the live process (UI projects from it, remove_rule indexes into it,
        // Session scope lives only here). Duplicates here are visible (UI
        // shows copies) and break remove_rule semantics (delete one row ->
        // remove all matches on disk but leave the rest in memory).
        let mut rules = self.rules.lock().expect("mode gate mutex");
        if rules.iter().all(|r| !r.same_as(&rule)) {
            rules.push(rule);
        } else {
            tracing::debug!(
                action = %rule.action,
                scope = ?rule.scope,
                "[permission] add_rule: duplicate rule skipped in memory (no push)"
            );
        }
    }

    fn add_directory(&self, dir: &std::path::Path, scope: crate::store::Scope) {
        // Persist a path-bounds approval marked "always" so the fence
        // rehydrates the directory on restart. The fence's additional_dirs is
        // updated separately (by the server, before resume). Log a store
        // failure rather than silently dropping — matches add_rule.
        if scope.is_writable()
            && let Some(store) = &self.store
            && let Err(e) = store.add_directory(dir, scope)
        {
            tracing::warn!(
                "[permission] add_directory: persistence write failed; the \
                 directory auth will not survive restart: {e}"
            );
        }
    }

    fn remove_rule(&self, index: usize) -> bool {
        // Drop the rule from memory under the lock, then persist the deletion
        // OUTSIDE the mutex. store.remove does a tmp+rename disk write; holding
        // the rules mutex across it would stall decide() calls from a
        // concurrent run (the UI deleting a rule while the agent's tool call
        // is being gated). The index is against the WRITABLE (durable) rules
        // only — builtin rules ship with the binary and are not user-removable
        // here, so the index the /permissions list shows (which filters
        // builtins out) matches the rule this removes.
        let removed = {
            let mut rules = self.rules.lock().expect("mode gate mutex");
            let durable_idx = rules
                .iter()
                .enumerate()
                .filter(|(_, r)| r.scope.is_writable())
                .nth(index)
                .map(|(i, _)| i);
            match durable_idx {
                Some(i) => rules.remove(i),
                None => return false,
            }
        };
        if let Some(store) = &self.store {
            // Full-identity delete (action + content + effect + scope) so a
            // same-action sibling (bash npm:* vs bash git:*) is not wiped.
            if let Err(e) = store.remove(&removed) {
                // Do NOT silently drop: the in-memory rule is gone but the
                // disk write failed, so the rule re-hydrates on restart. The
                // eprintln is a first-step signal (TODO route the failure
                // back through the server as a ResponsePayload::Error so the
                // TUI can surface it — eprintln is invisible under the TUI's
                // raw-mode alt screen, but it at least leaves a trace in the
                // launching shell).
                tracing::warn!(
                    "[permission] remove_rule: in-memory rule deleted but the \
                     persistence write failed; it will re-hydrate on restart: {e}"
                );
            }
        }
        true
    }

    fn set_git_checkpoint_enabled(&self, enabled: bool) {
        // Toggle the four git-checkpoint builtin rule ids in the disabled set,
        // then re-seed the rule set: drop the old builtins and prepend the
        // non-disabled ones so they stay ahead of persisted rules (last-match
        // wins, deny wins — a user allow rule still shadows the builtin ask).
        let ids: Vec<String> = crate::rule::builtin_rules()
            .iter()
            .filter_map(crate::rule::builtin_rule_id)
            .collect();
        let mut disabled = self
            .disabled_builtins
            .lock()
            .expect("disabled_builtins mutex");
        if enabled {
            for id in &ids {
                disabled.remove(id);
            }
        } else {
            for id in &ids {
                disabled.insert(id.clone());
            }
        }
        let disabled_snapshot = disabled.clone();
        drop(disabled);
        let mut rules = self.rules.lock().expect("mode gate mutex");
        let kept: Vec<Rule> = rules
            .iter()
            .filter(|r| {
                crate::rule::builtin_rule_id(r)
                    .map(|id| !disabled_snapshot.contains(&id))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        let mut seeded: Vec<Rule> = crate::rule::builtin_rules()
            .into_iter()
            .filter(|r| {
                crate::rule::builtin_rule_id(r)
                    .map(|id| !disabled_snapshot.contains(&id))
                    .unwrap_or(true)
            })
            .collect();
        seeded.extend(kept);
        *rules = seeded;
    }

    fn git_checkpoint_enabled(&self) -> bool {
        let disabled = self
            .disabled_builtins
            .lock()
            .expect("disabled_builtins mutex");
        let ids: Vec<String> = crate::rule::builtin_rules()
            .iter()
            .filter_map(crate::rule::builtin_rule_id)
            .collect();
        // Enabled when none of the git-checkpoint ids are disabled.
        !ids.iter().any(|id| disabled.contains(id))
    }

    fn decision_metrics(&self) -> Vec<crate::metrics::DecisionBucket> {
        self.metrics.snapshot()
    }
}

/// Public classifier for the git-confirm checkpoint: the git subcommand word
/// (commit/rebase/reset/tag) when the content is a checkpoint op, else None.
/// The service boundary uses this to route a session-scope approval to the
/// gate's session consent rather than a persistent allow rule.
pub fn classify_git_op(tool_name: &str, content: &str) -> Option<&'static str> {
    crate::git_discard::should_ask_before_git(tool_name, content)
}

/// A per-mode strategy that decides Allow / Ask / Deny for a tool request when
/// no rule, safety check, or consent matched. Each mode has its own policy
/// struct so the behavior is isolated and testable. The dispatch in mode_default
/// picks the struct for the current mode.
///
/// The policy is a pure function of the request and its pre-computed
/// side-effect level — no I/O, no mutable state, no classifier. The pipeline
/// (rules, safety, compound, destructive, consent) runs before the policy;
/// the policy only handles the residual case where nothing else decided.
pub trait ModePolicy {
    /// The verdict for a request with the given side-effect level.
    fn decide(&self, req: &ToolRequest, se: SideEffect) -> Decision;
}

/// Manual mode: least-privilege. Pure reads auto-allow unless the tool
/// declares it needs approval; everything else escalates to Ask.
pub struct DefaultPolicy;

impl ModePolicy for DefaultPolicy {
    fn decide(&self, req: &ToolRequest, se: SideEffect) -> Decision {
        match se {
            SideEffect::None => {
                if req.native_requires_approval {
                    Decision::Ask(mode_ask_reason("tool declares it needs approval"))
                } else {
                    Decision::Allow(AllowReason::ModeDefault)
                }
            }
            _ => Decision::Ask(mode_ask_reason("manual mode asks before this side effect")),
        }
    }
}

/// Auto mode: execution and filesystem writes are allowed (the destructive
/// and compound gates in the pipeline already caught dangerous commands);
/// pure reads auto-allow unless the tool declares it needs approval; network
/// still escalates to Ask.
pub struct AutoPolicy;

impl ModePolicy for AutoPolicy {
    fn decide(&self, req: &ToolRequest, se: SideEffect) -> Decision {
        match se {
            SideEffect::Exec => Decision::Allow(AllowReason::ModeDefault),
            SideEffect::Filesystem => Decision::Allow(AllowReason::ModeDefault),
            SideEffect::None => {
                if req.native_requires_approval {
                    Decision::Ask(mode_ask_reason("tool declares it needs approval"))
                } else {
                    Decision::Allow(AllowReason::ModeDefault)
                }
            }
            _ => Decision::Ask(mode_ask_reason("auto mode asks before network egress")),
        }
    }
}

/// Build the reason the mode-default validator attaches to an Ask it produces.
/// Every mode-default ask shares the same source and validator name; only the
/// one-sentence detail varies.
fn mode_ask_reason(detail: &str) -> AskReason {
    AskReason {
        source: AskSource::ToolNative,
        validator: crate::pipeline::mode_default::MODE_DEFAULT,
        detail: detail.into(),
        containment_note: None,
    }
}

/// The per-mode default policy when no rule matches, no safety check fires,
/// and no consent is stored. Dispatches to the ModePolicy struct for the
/// given mode. The side-effect level is computed from the tool name and drives
/// the baseline: under Manual, None auto-allows (pure reads) and Exec /
/// Network / Filesystem escalate to Ask; Auto relaxes Exec and Filesystem to
/// Allow so the autonomous edit loop is usable (the pipeline's destructive and
/// compound gates already caught dangerous commands). A tool's own
/// requires_approval flag still escalates a None side-effect tool to Ask when
/// the tool declares it needs a human.
pub fn mode_default(mode: PermissionMode, req: &ToolRequest) -> Decision {
    let se = side_effect_for(req.tool_name);
    match mode {
        // Manual: ask before tools that need approval; read-only auto-allows.
        PermissionMode::Manual => DefaultPolicy.decide(req, se),
        // Auto: allow safe ops, ask destructive (until the recoverable
        // invariant replaces the destructive ask).
        PermissionMode::Auto => AutoPolicy.decide(req, se),
    }
}

#[cfg(test)]
mod tests;
