//! Stable block identity for the transcript: a turn-level grouping of frames
//! whose identity survives rebuild and (later) frame-window eviction. The
//! rebuild reuses a block whose anchor and revision are unchanged, keeping
//! its derived lines and expand state without re-pairing them by content.

use std::ops::Range;

use super::turn_fold::TurnCheckpoint;
use crate::records::TranscriptLine;
use crate::row_id::{LocalEntryId, RowId};
use crate::transcript::rows::NamedRows;

/// The stable identity of a block. A block that covers frames anchors on its
/// opening frame: a frame from a durable log entry takes that entry's row
/// identity, which survives a re-read of the log; a frame the frontend raised
/// takes its absolute log position, stable while the frame stays in the
/// visible window. The stream seq is deliberately not an anchor source —
/// numbering restarts with the service process, so an identity built from it
/// would drift.
///
/// The block holding the window's front frames has no opening frame of its
/// own to name — its first frame moves as the window slides back — so it takes
/// a name from its place in the list. No two blocks share a name: a
/// frame-derived name belongs to one frame, and the front slot is the only one
/// of its kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum BlockAnchor {
    Durable(RowId),
    Local(u64),
    /// The window's front slot, whose frames precede the turn now being
    /// folded. The slot keeps its name while the window slides under it.
    Frozen,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BlockId(pub BlockAnchor);

/// One turn-level slice of the frame log, with the lines the fold derived
/// for it — each beside the identity of the frame that produced it — a
/// revision that ticks when its frames change, and the state the slice's
/// frames ended inside a turn with. Rebuild reuses a block whose id and range
/// still describe the same frames and whose turn state still agrees with the
/// frame that decides how its last message reads, carrying its lines, expand
/// state, and turn state forward without re-deriving them; the turn state is
/// what lets the block after this one continue a turn this one opened and did
/// not close.
#[derive(Debug, Clone)]
pub(crate) struct Block {
    pub id: BlockId,
    pub frame_range: Range<usize>,
    /// The rows the fold derived for this slice, each kept beside the identity
    /// of the frame that produced it: an older-row read matches its overlap
    /// against those identities, and the visible list takes both together.
    pub rows: NamedRows,
    pub revision: u64,
    pub checkpoint: TurnCheckpoint,
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
            for l in b.rows.lines_mut().iter_mut() {
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
    for new_line in new.rows.lines_mut().iter_mut() {
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
        if let Some(old_rows) = old.rows.lines().iter().find_map(|l| match l {
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
mod tests;
