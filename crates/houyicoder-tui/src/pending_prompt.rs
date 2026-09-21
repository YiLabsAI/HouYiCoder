//! The reverse request awaiting a human verdict: a tool approval with its
//! optional interactive question card, or a startup workspace-trust ask. App
//! reads the pending ask through the accessors below, beside the type.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::run::ApprovalRequest;
use houyicoder_protocol::frontend::trust::TrustPrompt;

use crate::ask_question_model::AskQuestion;
use crate::records::Approval;
use crate::state::App;
use crate::state::enums::TrustChoice;

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

    /// Take the interactive question card, keeping the request id so a later
    /// resolve can still ship the verdict against it.
    pub fn take_question(&mut self) -> Option<AskQuestion> {
        match self {
            Self::Permission { question, .. } => question.take().map(|b| *b),
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
    pub(crate) fn approval(&self) -> Option<&Approval> {
        self.prompt.as_ref().and_then(|p| p.approval())
    }

    /// Mutably borrow the generic approval card, or None when not a plain
    /// permission ask.
    pub(crate) fn approval_mut(&mut self) -> Option<&mut Approval> {
        self.prompt.as_mut().and_then(|p| p.approval_mut())
    }

    /// The interactive question card, or None for a plain approval or trust.
    pub(crate) fn ask_question(&self) -> Option<&AskQuestion> {
        self.prompt.as_ref().and_then(|p| p.question())
    }

    /// Mutably borrow the interactive question card, or None when not a
    /// question ask.
    pub(crate) fn ask_question_mut(&mut self) -> Option<&mut AskQuestion> {
        self.prompt.as_mut().and_then(|p| p.question_mut())
    }

    /// Take the interactive question card, keeping the request id for a later
    /// resolve.
    pub(crate) fn take_ask_question(&mut self) -> Option<AskQuestion> {
        self.prompt.as_mut().and_then(|p| p.take_question())
    }

    /// The startup trust prompt, or None for a permission ask.
    pub(crate) fn pending_trust(&self) -> Option<&TrustPrompt> {
        self.prompt.as_ref().and_then(|p| p.trust())
    }

    /// The selected trust action, or Accept for a permission ask.
    pub(crate) fn trust_choice(&self) -> TrustChoice {
        self.prompt
            .as_ref()
            .map_or(TrustChoice::Accept, |p| p.choice())
    }

    /// Replace the selected trust action.
    pub(crate) fn set_trust_choice(&mut self, choice: TrustChoice) {
        if let Some(p) = self.prompt.as_mut() {
            p.set_choice(choice);
        }
    }
}
