//! Identity of transcript rows.
//!
//! The same durable log is read twice — from the live stream and from a
//! snapshot on disk — and a row keeps its identity across both reads only
//! when the name comes from the entry that produced it, not from the window
//! the read started at. A row no entry names falls back to its absolute
//! frame position; both read paths derive identities through one function
//! here, so the slot rule has a single definition.

use std::fmt;

use houyicoder_protocol::envelope::DurableEventId;

use crate::transcript::TranscriptFrame;

/// The identity of a row no durable entry named: an in-process monotonic
/// number, taken from the row's absolute frame position, which stays put
/// when the resident window's front drains. Not persisted, and not
/// comparable across processes — a restart begins a new view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalEntryId(pub u64);

/// The identity of one transcript row. The two forms cannot collide: the
/// durable form displays with the entry marker and a slot, the local form
/// with the frame marker.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowId {
    /// A row derived from a durable log entry: the entry's identity plus the
    /// frame's position in the entry's canonical frame order. One entry
    /// yields at most one frame per order position, so an event and a slot
    /// name one row.
    Durable { event: DurableEventId, slot: u8 },
    /// A row no durable entry named: a frame the frontend raised, or a frame
    /// read from a log whose entries carry no identities.
    Local(LocalEntryId),
}

impl RowId {
    /// The identity of the row a frame produces. A frame that came from a
    /// durable log entry takes that entry's identity and the frame's slot in
    /// the canonical frame order; a frame no entry named takes the position
    /// the caller supplies. Both read paths call this, so the same entry
    /// names the same row in every read.
    pub fn of(frame: &TranscriptFrame, event: Option<&DurableEventId>, local: u64) -> RowId {
        match event {
            // A frame the frontend raised has no category in the frame order,
            // and the host stamps identities only on frames it derived from
            // the log, so the caller's position is the only name available.
            Some(event) if !matches!(frame, TranscriptFrame::Frontend(_)) => RowId::Durable {
                event: event.clone(),
                slot: slot_of(frame),
            },
            _ => RowId::local(local),
        }
    }

    /// A local identity for a row no durable entry named, at this absolute
    /// frame position.
    pub fn local(position: u64) -> RowId {
        RowId::Local(LocalEntryId(position))
    }

    /// Whether the identity comes from a durable log entry, and so names the
    /// same row in every read of that entry; a local identity names a
    /// position in one frame log and means nothing in another.
    pub fn is_durable(&self) -> bool {
        matches!(self, RowId::Durable { .. })
    }

    /// Whether this identity names the row a frame produces: the frame
    /// carries exactly this entry identity and its category sits at this
    /// slot. A local identity never names a frame — frames carry entry
    /// identities, not counter values — so a scan for a local identity
    /// finds nothing without a comparison that could falsely match.
    pub fn names(&self, frame: &TranscriptFrame, event: Option<&DurableEventId>) -> bool {
        match self {
            RowId::Durable { event: own, slot } => event == Some(own) && *slot == slot_of(frame),
            RowId::Local(_) => false,
        }
    }
}

/// The position of a frame's category in the canonical frame order
/// [session update, acpx notification]: the order the host writes an entry's
/// payloads in, which the snapshot reader replays. The slot is the category
/// position, not a count of the frames the entry produced — an entry whose
/// only frame is a notification still takes the notification's slot, so the
/// two readers agree without either counting the other's output.
fn slot_of(frame: &TranscriptFrame) -> u8 {
    match frame {
        TranscriptFrame::Session(_) => 0,
        TranscriptFrame::Acpx(_) => 1,
        // A frontend-raised frame has no category in the frame order. of()
        // filters the category out before the slot is read, and names()
        // compares entry identities first, which such a frame never carries,
        // so neither path reads this value.
        TranscriptFrame::Frontend(_) => 0,
    }
}

impl fmt::Display for RowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowId::Durable { event, slot } => write!(f, "e{}.{}", event.0, slot),
            RowId::Local(LocalEntryId(n)) => write!(f, "f{n}"),
        }
    }
}

impl From<&str> for RowId {
    /// Names the leading slot of an event's rows. The read paths
    /// derive identities through of(); this conversion is for callers that
    /// already hold an event name. It is not a parser: a rendered identity
    /// carries a slot this conversion cannot read, so feeding display output
    /// back through it yields a different identity rather than the original.
    fn from(event: &str) -> Self {
        RowId::Durable {
            event: DurableEventId(event.to_string()),
            slot: 0,
        }
    }
}

impl From<String> for RowId {
    /// Names the leading slot of an event's rows. The read paths
    /// derive identities through of(); this conversion is for callers that
    /// already hold an event name. It is not a parser: a rendered identity
    /// carries a slot this conversion cannot read, so feeding display output
    /// back through it yields a different identity rather than the original.
    fn from(event: String) -> Self {
        RowId::Durable {
            event: DurableEventId(event),
            slot: 0,
        }
    }
}

#[cfg(test)]
mod tests;
