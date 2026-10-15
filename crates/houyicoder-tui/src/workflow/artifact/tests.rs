//! The artifact domain's own tests: the proposer contract and the
//! session's line-level edits.

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

mod session;
