//! Artifact closed-loop module (TUI-local). The session aggregate, proposer
//! trait, and StubProposer live here — the TUI no longer imports the artifact
//! domain from the engine. The session submodule holds the aggregate + the
//! ChangeProposer trait; this module adds the deterministic StubProposer used
//! by the prototype App.

mod session;

pub use session::{
    Annotation, AppliedChange, ArtifactMode, ArtifactSession, ChangeProposer, ProposedChange,
    TuiError,
};

/// Deterministic stub proposer (no LLM). It cannot interpret natural-language
/// text, so propose always returns Ok(None): the NL path surfaces a hint that
/// a real LLM-backed proposer is not wired, and the user is directed to the
/// direct edit keys (c/o/d). The trait is the seam for a real proposer; this
/// impl exists to close the prototype loop without a model dependency.
#[derive(Debug, Default, Clone)]
pub struct StubProposer;

impl StubProposer {
    /// Construct a stub proposer.
    pub fn new() -> Self {
        Self
    }
}

impl ChangeProposer for StubProposer {
    fn propose(
        &self,
        _session: &ArtifactSession,
        _annotation: &Annotation,
    ) -> Result<Option<ProposedChange>, TuiError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proposer_nl_returns_none() {
        let mut s = ArtifactSession::stub();
        let ann = s
            .push_annotation("rewrite this line to be clearer".to_string())
            .expect("stub session has content to annotate");
        let proposal = StubProposer::new().propose(&s, &ann).unwrap();
        assert!(proposal.is_none());
    }

    #[test]
    fn test_change_proposer_is_object() {
        let _boxed: Box<dyn ChangeProposer> = Box::new(StubProposer);
    }
}
