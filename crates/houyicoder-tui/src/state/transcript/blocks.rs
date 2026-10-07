//! Stable block identity for the transcript: a turn-level grouping of frames
//! whose identity survives rebuild and (later) frame-window eviction. The
//! rebuild reuses a block whose anchor and revision are unchanged, keeping
//! its derived lines and expand state without re-pairing them by content.

use std::ops::Range;

use crate::records::TranscriptLine;
use crate::row_id::{LocalEntryId, RowId};

/// The stable identity of a block, taken from its opening frame. A frame
/// from a durable log entry anchors the block on that entry's row identity,
/// which survives a re-read of the log; a frame the frontend raised anchors
/// on its absolute log position, stable while the frame stays in the visible
/// window. The stream seq is deliberately not an anchor source — numbering
/// restarts with the service process, so an identity built from it would
/// drift.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum BlockAnchor {
    Durable(RowId),
    Local(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BlockId(pub BlockAnchor);

/// One turn-level slice of the frame log, with the lines the projection
/// derived for it and a revision that ticks when its frames change. Rebuild
/// reuses a block whose id and revision match, carrying its lines and expand
/// state forward without re-deriving them.
#[derive(Debug, Clone)]
pub(crate) struct Block {
    pub id: BlockId,
    pub frame_range: Range<usize>,
    pub lines: Vec<TranscriptLine>,
    pub revision: u64,
}

/// The delta vocabulary the rebuild applies to the block list. Evict before a
/// survivor when the cap advances past aged-out blocks; toggle and other
/// variants arrive in later steps as their producers land.
#[derive(Debug, Clone)]
pub(crate) enum TranscriptChange {
    AppendBlock(Block),
    ReplaceBlock { id: BlockId, block: Block },
    RewindTo { id: BlockId },
    EvictBefore { id: BlockId },
}

#[derive(Debug, Default, Clone)]
pub(crate) struct TranscriptChangeSet {
    pub changes: Vec<TranscriptChange>,
}

impl TranscriptChangeSet {
    pub(crate) fn push(&mut self, change: TranscriptChange) {
        self.changes.push(change);
    }
}

/// The ordered, reusable turn-level blocks the transcript keeps between
/// rebuilds. A block anchors on its opening frame's log position; the
/// position is stable while the block stays in the visible window, which is
/// the only place it is reused.
#[derive(Debug, Default)]
pub(crate) struct TranscriptBlocks {
    blocks: Vec<Block>,
}

impl TranscriptBlocks {
    pub(crate) fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub(crate) fn clear(&mut self) {
        self.blocks.clear();
    }

    /// Swap fetched child rows into the Subagent line a block holds for the
    /// named child session. A rebuild re-derives a block's lines from its
    /// frames (empty folded_transcript), and migrate_fetched_rows carries the
    /// rows forward from the block it replaces, so the fill must reach the
    /// block, not just the flattened view, or the next rebuild loses them.
    pub(crate) fn set_subagent_folded(&mut self, child_sid: &str, folded: &[TranscriptLine]) {
        for b in self.blocks.iter_mut() {
            for l in b.lines.iter_mut() {
                if let TranscriptLine::Subagent {
                    child_sid: c,
                    folded_transcript,
                    ..
                } = l
                    && c.as_str() == child_sid
                {
                    folded_transcript.clear();
                    folded_transcript.extend_from_slice(folded);
                }
            }
        }
    }

    /// The stable id for a block whose opening frame carries the given row
    /// identity. A durable identity anchors the block on its log entry, so
    /// the block keeps its id across a re-read; a local identity anchors on
    /// the frame's absolute log position, which stays put while the frame
    /// stays in the visible window — exactly the span a block is reused
    /// across.
    pub(crate) fn assign_id(&self, opening: RowId) -> BlockId {
        BlockId(match opening {
            id @ RowId::Durable { .. } => BlockAnchor::Durable(id),
            RowId::Local(LocalEntryId(n)) => BlockAnchor::Local(n),
        })
    }

    /// Apply a change set to the block list. ReplaceBlock migrates fetched
    /// child rows from the block it replaces by matching child session id,
    /// so a re-derived subagent block keeps the rows an earlier fetch filled
    /// rather than rendering empty until the next fetch.
    pub(crate) fn apply(&mut self, changes: TranscriptChangeSet) {
        for change in changes.changes {
            match change {
                TranscriptChange::AppendBlock(block) => self.blocks.push(block),
                TranscriptChange::ReplaceBlock { id, mut block } => {
                    if let Some(old) = self.blocks.iter().find(|b| b.id == id) {
                        migrate_fetched_rows(old, &mut block);
                    }
                    if let Some(slot) = self.blocks.iter_mut().find(|b| b.id == id) {
                        *slot = block;
                    }
                }
                TranscriptChange::RewindTo { id } => {
                    if let Some(pos) = self.blocks.iter().position(|b| b.id == id) {
                        self.blocks.truncate(pos + 1);
                    }
                }
                TranscriptChange::EvictBefore { id } => {
                    if let Some(pos) = self.blocks.iter().position(|b| b.id == id) {
                        self.blocks.drain(..pos);
                    }
                }
            }
        }
    }
}

/// Copy fetched child rows from the old block onto the new one where the
/// same child session appears. A subagent line the new block rendered empty
/// (no fetch yet this rebuild) takes the rows the old block already fetched,
/// so the view does not blank out between rebuilds of the same delegation.
fn migrate_fetched_rows(old: &Block, new: &mut Block) {
    for new_line in new.lines.iter_mut() {
        let TranscriptLine::Subagent {
            child_sid,
            folded_transcript,
            ..
        } = new_line
        else {
            continue;
        };
        if !folded_transcript.is_empty() {
            continue;
        }
        if let Some(old_rows) = old.lines.iter().find_map(|l| match l {
            TranscriptLine::Subagent {
                child_sid: os,
                folded_transcript: oft,
                ..
            } if os.as_str() == child_sid.as_str() => Some(oft),
            _ => None,
        }) {
            folded_transcript.extend_from_slice(old_rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::TranscriptLine;

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
            lines,
            revision: 0,
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
                BlockAnchor::Durable(_) => panic!("test blocks anchor locally"),
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
        let carried = match new.lines.first() {
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
        let carried = match new.lines.first() {
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
            Some(Block { lines, .. }) => {
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
                BlockAnchor::Durable(_) => panic!("test blocks anchor locally"),
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
}
