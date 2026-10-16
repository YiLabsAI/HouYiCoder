use super::*;
use crate::records::TranscriptLine;
use crate::transcript::TurnCheckpoint;
use crate::transcript::rows::NamedRows;

fn subagent(child_sid: &str, rows: Vec<TranscriptLine>) -> TranscriptLine {
    TranscriptLine::Subagent {
        child_sid: child_sid.to_string(),
        subagent_type: "task".to_string(),
        summary: "summary".to_string(),
        prompt: String::new(),
        folded_transcript: rows,
        color: None,
    }
}

fn block(id: u64, lines: Vec<TranscriptLine>) -> Block {
    Block {
        id: BlockId(BlockAnchor::Local(id)),
        frame_range: 0..1,
        rows: NamedRows::from_lines(lines),
        stand_in_rows: 0,
        revision: 0,
        checkpoint: TurnCheckpoint::default(),
    }
}

/// EvictBefore drops every block before the named id and keeps the rest in
/// order, so a frame-window advance trims only the aged-out prefix.
#[test]
fn test_evict_drops_prefix() {
    let mut bs = TranscriptBlocks::default();
    bs.apply(TranscriptChangeSet {
        changes: vec![
            TranscriptChange::AppendBlock(block(1, vec![])),
            TranscriptChange::AppendBlock(block(2, vec![])),
            TranscriptChange::AppendBlock(block(3, vec![])),
        ],
    });
    bs.apply(TranscriptChangeSet {
        changes: vec![TranscriptChange::EvictBefore {
            id: BlockId(BlockAnchor::Local(2)),
        }],
    });
    let ids: Vec<u64> = bs
        .blocks()
        .iter()
        .map(|b| match b.id.0 {
            BlockAnchor::Local(n) => n,
            BlockAnchor::Durable(_) | BlockAnchor::Frozen => {
                panic!("test blocks anchor on a frame")
            }
        })
        .collect();
    assert_eq!(ids, vec![2, 3]);
}

/// A re-derived subagent block keeps the fetched child rows the old block
/// already held, matched by child session id, so the view does not blank
/// out between rebuilds of the same delegation.
#[test]
fn test_migrate_rows_by_sid() {
    let fetched = vec![TranscriptLine::Agent("child work".into())];
    let old = block(1, vec![subagent("sid-a", fetched.clone())]);
    let mut new = block(1, vec![subagent("sid-a", vec![])]);
    migrate_fetched_rows(&old, &mut new);
    let carried = match new.rows.lines().first() {
        Some(TranscriptLine::Subagent {
            folded_transcript, ..
        }) => folded_transcript.clone(),
        _ => vec![],
    };
    assert_eq!(carried.len(), fetched.len());
    assert!(matches!(
        carried.first(),
        Some(TranscriptLine::Agent(t)) if t == "child work"
    ));
}

/// A child the old block never fetched stays empty: migrate does not invent
/// rows for a delegation the old block did not serve.
#[test]
fn test_migrate_skips_unknown() {
    let old = block(1, vec![subagent("sid-a", vec![])]);
    let mut new = block(1, vec![subagent("sid-b", vec![])]);
    migrate_fetched_rows(&old, &mut new);
    let carried = match new.rows.lines().first() {
        Some(TranscriptLine::Subagent {
            folded_transcript, ..
        }) => folded_transcript.clone(),
        _ => vec![],
    };
    assert!(carried.is_empty());
}

/// ReplaceBlock lands the new block at the same id, so a turn whose frames
/// changed re-derives in place without disturbing the prefix order.
#[test]
fn test_replace_block_in_place() {
    let mut bs = TranscriptBlocks::default();
    bs.apply(TranscriptChangeSet {
        changes: vec![
            TranscriptChange::AppendBlock(block(1, vec![TranscriptLine::Agent("old".into())])),
            TranscriptChange::AppendBlock(block(2, vec![])),
        ],
    });
    bs.apply(TranscriptChangeSet {
        changes: vec![TranscriptChange::ReplaceBlock {
            id: BlockId(BlockAnchor::Local(1)),
            block: block(1, vec![TranscriptLine::Agent("new".into())]),
        }],
    });
    match bs.blocks().first() {
        Some(Block { rows, .. }) => {
            let lines = rows.lines();
            assert_eq!(lines.len(), 1);
            assert!(matches!(lines.first(), Some(TranscriptLine::Agent(t)) if t == "new"));
        }
        _ => panic!("first block present"),
    }
    assert_eq!(bs.blocks().len(), 2);
}

/// RewindTo truncates the tail past the named id: a frame-log rewind drops
/// the blocks whose frames were removed and keeps the prefix intact.
#[test]
fn test_rewind_truncates_tail() {
    let mut bs = TranscriptBlocks::default();
    bs.apply(TranscriptChangeSet {
        changes: vec![
            TranscriptChange::AppendBlock(block(1, vec![])),
            TranscriptChange::AppendBlock(block(2, vec![])),
            TranscriptChange::AppendBlock(block(3, vec![])),
        ],
    });
    bs.apply(TranscriptChangeSet {
        changes: vec![TranscriptChange::RewindTo {
            id: BlockId(BlockAnchor::Local(1)),
        }],
    });
    let ids: Vec<u64> = bs
        .blocks()
        .iter()
        .map(|b| match b.id.0 {
            BlockAnchor::Local(n) => n,
            BlockAnchor::Durable(_) | BlockAnchor::Frozen => {
                panic!("test blocks anchor on a frame")
            }
        })
        .collect();
    assert_eq!(ids, vec![1]);
}

/// assign_id anchors a block on the identity its opening frame carries:
/// a durable entry keeps the block id stable across a re-read of the log,
/// a frontend frame falls back to the frame's log position, stable while
/// the frame stays in the window.
#[test]
fn test_assign_id_anchors() {
    let bs = TranscriptBlocks::default();
    assert_eq!(
        bs.assign_id(RowId::local(5)),
        BlockId(BlockAnchor::Local(5))
    );
    let durable: RowId = "e1".into();
    assert_eq!(
        bs.assign_id(durable.clone()),
        BlockId(BlockAnchor::Durable(durable))
    );
}
