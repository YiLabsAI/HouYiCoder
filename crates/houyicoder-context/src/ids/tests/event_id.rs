use super::super::*;

/// Ids minted back to back land in the same millisecond, which is exactly the
/// window a clock-only id leaves to chance. The descriptor keeps the prompt
/// with the newest event id, so an id that sorts behind an earlier one would
/// move a session's subject back to an older statement.
#[test]
fn test_event_ids_ascend() {
    let ids: Vec<EventId> = (0..64).map(|_| EventId::new()).collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "ids minted back to back sort in mint order"
    );
}
