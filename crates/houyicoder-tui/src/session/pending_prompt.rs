//! The reverse request awaiting a human verdict: a tool approval with its
//! optional interactive question card, or a startup workspace-trust ask. App
//! reads the pending ask through the accessors below, beside the type.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::frontend::permission::PermissionMode;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest};
use houyicoder_protocol::frontend::trust::TrustPrompt;

use crate::agent_message::ClientCommand;
use crate::ask_question_model::AskQuestion;
use crate::records::Approval;
use crate::state::App;
use crate::state::enums::{LiveBlock, TrustChoice};

/// A reverse request the host is waiting on, of one of two forms. At most one
/// is live at a time. An interactive question is a permission whose card is
/// the question form, so both live under Permission and share one request id.
#[derive(Debug)]
pub enum PendingPrompt {
    /// A tool approval while the run is paused in Waiting. Card and question
    /// are mutually exclusive by convention, not by the field types.
    Permission {
        req_id: RequestId,
        requests: Vec<ApprovalRequest>,
        card: Option<Approval>,
        question: Option<Box<AskQuestion>>,
    },
    /// A startup workspace-trust ask, shown before any run begins.
    Trust {
        req_id: RequestId,
        prompt: TrustPrompt,
        choice: TrustChoice,
    },
}

impl PendingPrompt {
    /// A permission ask awaiting its card or question, holding the batch of
    /// approval requests the verdict applies to.
    pub fn permission(req_id: RequestId, requests: Vec<ApprovalRequest>) -> Self {
        Self::Permission {
            req_id,
            requests,
            card: None,
            question: None,
        }
    }

    /// A permission ask carrying only a generic approval card.
    pub fn approval_card(req_id: RequestId, card: Approval) -> Self {
        Self::Permission {
            req_id,
            requests: Vec::new(),
            card: Some(card),
            question: None,
        }
    }

    /// A permission ask carrying only an interactive question card.
    pub fn question_card(req_id: RequestId, question: AskQuestion) -> Self {
        Self::Permission {
            req_id,
            requests: Vec::new(),
            card: None,
            question: Some(Box::new(question)),
        }
    }

    /// A startup workspace-trust ask, with Accept preselected.
    pub fn trust_ask(req_id: RequestId, prompt: TrustPrompt) -> Self {
        Self::Trust {
            req_id,
            prompt,
            choice: TrustChoice::Accept,
        }
    }

    /// The reverse-request id the verdict answers.
    pub fn req_id(&self) -> RequestId {
        match self {
            Self::Permission { req_id, .. } | Self::Trust { req_id, .. } => *req_id,
        }
    }

    /// Whether this ask pauses a run; a startup trust ask leaves the run idle.
    pub fn is_permission(&self) -> bool {
        matches!(self, Self::Permission { .. })
    }

    /// How many approval requests this ask batches, zero for a trust ask.
    pub fn request_count(&self) -> usize {
        match self {
            Self::Permission { requests, .. } => requests.len(),
            Self::Trust { .. } => 0,
        }
    }

    /// The generic approval card, or None for a question or trust ask.
    pub fn approval(&self) -> Option<&Approval> {
        match self {
            Self::Permission { card, .. } => card.as_ref(),
            Self::Trust { .. } => None,
        }
    }

    /// Mutably borrow the generic approval card, or None when not a plain
    /// permission ask.
    pub fn approval_mut(&mut self) -> Option<&mut Approval> {
        match self {
            Self::Permission { card, .. } => card.as_mut(),
            Self::Trust { .. } => None,
        }
    }

    /// The interactive question card, or None for a plain approval or trust.
    pub fn question(&self) -> Option<&AskQuestion> {
        match self {
            Self::Permission { question, .. } => question.as_deref(),
            Self::Trust { .. } => None,
        }
    }

    /// Mutably borrow the interactive question card, or None when not a
    /// question ask.
    pub fn question_mut(&mut self) -> Option<&mut AskQuestion> {
        match self {
            Self::Permission { question, .. } => question.as_deref_mut(),
            Self::Trust { .. } => None,
        }
    }

    /// Replace the generic approval card, or do nothing on a non-permission
    /// ask.
    pub fn set_approval(&mut self, card: Option<Approval>) {
        if let Self::Permission { card: slot, .. } = self {
            *slot = card;
        }
    }

    /// Replace the interactive question card, or do nothing on a
    /// non-permission ask.
    pub fn set_question(&mut self, question: Option<AskQuestion>) {
        if let Self::Permission { question: slot, .. } = self {
            *slot = question.map(Box::new);
        }
    }

    /// The startup trust prompt, or None for a permission ask.
    pub fn trust(&self) -> Option<&TrustPrompt> {
        match self {
            Self::Trust { prompt, .. } => Some(prompt),
            Self::Permission { .. } => None,
        }
    }

    /// The selected trust action, or Accept when this is a permission ask.
    pub fn choice(&self) -> TrustChoice {
        match self {
            Self::Trust { choice, .. } => *choice,
            Self::Permission { .. } => TrustChoice::Accept,
        }
    }

    /// Replace the selected trust action, or do nothing on a permission ask.
    pub fn set_choice(&mut self, choice: TrustChoice) {
        if let Self::Trust { choice: slot, .. } = self {
            *slot = choice;
        }
    }
}

impl App {
    /// The generic approval card, or None for a question or trust ask.
    pub fn approval(&self) -> Option<&Approval> {
        let prompt = &self.sessions.current.prompt;
        prompt.as_ref().and_then(|p| p.approval())
    }

    /// Mutably borrow the generic approval card, or None when not a plain
    /// permission ask.
    pub(crate) fn approval_mut(&mut self) -> Option<&mut Approval> {
        let prompt = &mut self.sessions.current.prompt;
        prompt.as_mut().and_then(|p| p.approval_mut())
    }

    /// The interactive question card, or None for a plain approval or trust.
    pub(crate) fn ask_question(&self) -> Option<&AskQuestion> {
        let prompt = &self.sessions.current.prompt;
        prompt.as_ref().and_then(|p| p.question())
    }

    /// Mutably borrow the interactive question card, or None when not a
    /// question ask.
    pub(crate) fn ask_question_mut(&mut self) -> Option<&mut AskQuestion> {
        let prompt = &mut self.sessions.current.prompt;
        prompt.as_mut().and_then(|p| p.question_mut())
    }

    /// The startup trust prompt, or None for a permission ask.
    pub(crate) fn pending_trust(&self) -> Option<&TrustPrompt> {
        let prompt = &self.sessions.current.prompt;
        prompt.as_ref().and_then(|p| p.trust())
    }

    /// The selected trust action, or Accept for a permission ask.
    pub(crate) fn trust_choice(&self) -> TrustChoice {
        let prompt = &self.sessions.current.prompt;
        prompt.as_ref().map_or(TrustChoice::Accept, |p| p.choice())
    }

    /// Replace the selected trust action.
    pub(crate) fn set_trust_choice(&mut self, choice: TrustChoice) {
        if let Some(p) = self.sessions.current.prompt.as_mut() {
            p.set_choice(choice);
        }
    }

    /// Resolve the active approval, clear its interface state, and send the
    /// verdict as the matching reverse response. No-op when no request awaits
    /// a decision.
    pub fn resolve_current_approval(&mut self, decision: ApprovalDecision) {
        let req_id = match self.sessions.current.prompt.as_ref() {
            Some(PendingPrompt::Permission { req_id, .. }) => *req_id,
            _ => return,
        };
        // Enqueue first: the card and the run-resume state only move once the
        // verdict actually reached the driver.
        if let Err(e) = self.enqueue(ClientCommand::Verdict { req_id, decision }) {
            self.system_line(Self::enqueue_failure_line("permission", e));
            return;
        }
        // Resume the run without resetting its clock: resolve_permission
        // flips Waiting -> Running preserving the original started_at.
        self.sessions.current.resolve_permission();
        self.refresh_fold_active();
        // Clear stale thinking state before post-resume streaming begins. The
        // run carried its progress through the pause, so these resets clear
        // real leftover values rather than touching defaults.
        if let Some(p) = self.run_progress_mut() {
            p.last_delta_at = None;
            p.live_block = LiveBlock::None;
            p.thinking_started_at = None;
        }
    }

    /// Resolve the startup trust verdict. Rejection also exits the local TUI,
    /// but only after the verdict was actually queued.
    pub fn resolve_trust(&mut self, accept: bool) {
        let req_id = match self.sessions.current.prompt.as_ref() {
            Some(PendingPrompt::Trust { req_id, .. }) => *req_id,
            _ => return,
        };
        if let Err(e) = self.enqueue(ClientCommand::TrustVerdict { req_id, accept }) {
            self.system_line(Self::enqueue_failure_line("trust", e));
            return;
        }
        self.sessions.current.clear_prompt();
        if !accept {
            self.ui.quit = true;
        }
    }

    /// Present a permission request and retain its request identifier.
    /// Question tools use the interactive question card; others use the
    /// generic approval card.
    pub(crate) fn raise_agent_approval(&mut self, ask: ApprovalRequest, req_id: RequestId) {
        let call_id = ask.call_id.clone();
        let tool = ask.tool_name.clone();
        // Pause the spinner while the run waits on the human verdict.
        // raise_permission enters Waiting and raises the ask in one step, so
        // the verdict can resume without resetting the clock.
        self.sessions
            .current
            .raise_permission(req_id, vec![ask.clone()]);
        self.refresh_fold_active();
        if tool == "AskUserQuestion"
            && let Some(aq) = AskQuestion::parse(&call_id, &ask.input)
        {
            if let Some(p) = self.sessions.current.prompt.as_mut() {
                p.set_question(Some(aq));
            }
            return;
        }
        // Malformed questions fall back to the generic card. Safety requests
        // hide persistent approval because consent cannot override them.
        let args = ask.input.to_string();
        let selected = self.initial_cursor(&tool);
        let (reason, source, containment_note) = if tool == ENTITLEMENT_TOOL {
            ("denied during the last command".to_string(), None, None)
        } else {
            match ask.reason {
                Some(r) => (r.detail.clone(), Some(r.source), r.containment_note.clone()),
                None => ("agent wants to run this tool".to_string(), None, None),
            }
        };
        let mut card = Approval {
            tool,
            args,
            reason,
            source,
            delegation: ask.delegation,
            containment_note,
            selected,
            call_id,
            options: ask.options,
        };
        // Two-option cards cannot retain a hidden persistent choice.
        if card.two_option_card() && card.selected == 2 {
            card.selected = 0;
        }
        if let Some(p) = self.sessions.current.prompt.as_mut() {
            p.set_approval(Some(card));
        }
    }

    /// Select the initial approval choice. A remembered verdict wins;
    /// automatic mode and the default both select one-time approval.
    pub(crate) fn initial_cursor(&self, tool: &str) -> usize {
        let permission = &self.sessions.current.capabilities.permission;
        if let Some(kind) = permission.sticky_choices.get(tool) {
            return Approval::index_for_kind(*kind);
        }
        if matches!(permission.mode_cache, Some(PermissionMode::Auto)) {
            return 0;
        }
        0
    }

    /// Whether a reverse request still awaits its verdict. Idle client
    /// requests pause so they cannot compete for response frames.
    pub fn reverse_request_in_flight(&self) -> bool {
        let prompt = &self.sessions.current.prompt;
        prompt.as_ref().is_some_and(|p| p.is_permission())
    }
}
