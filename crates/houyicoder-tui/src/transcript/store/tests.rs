use super::*;
use crate::records::ToolOutcome;
use crate::scroll::VIEWABLE_SCROLLBACK_CAP;
use crate::transcript::frame_payload::chunk_text;
use houyicoder_protocol::envelope::DurableEventId;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
use std::iter::repeat_with;

fn bash_call(cid: &str) -> TranscriptLine {
    TranscriptLine::Tool {
        name: "bash".into(),
        tool: "bash".into(),
        status: "ls".into(),
        invocation: "ls".into(),
        outcome: ToolOutcome::Success,
        call_id: cid.into(),
        body: String::new(),
        is_diff: false,
    }
}

fn bash_result(cid: &str) -> TranscriptLine {
    TranscriptLine::Tool {
        name: "result".into(),
        tool: "bash".into(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: cid.into(),
        body: String::new(),
        is_diff: false,
    }
}

/// A front drain past the scrollback cap shifts surviving fold groups down
/// by the dropped count, so the cache keeps pointing at the rows it
/// summarizes instead of a stale pre-drain index.
#[test]
fn test_trim_shift_fold_group() {
    let mut lines = vec![TranscriptLine::Agent("filler".into()); VIEWABLE_SCROLLBACK_CAP];
    lines.push(TranscriptLine::User("go".into()));
    lines.push(bash_call("c1"));
    lines.push(bash_result("c1"));
    let mut t = Transcript::from(lines);
    let call_idx = VIEWABLE_SCROLLBACK_CAP + 1;
    assert!(
        t.fold_groups().iter().any(|g| g.start == call_idx),
        "bash pair forms a group at the call index"
    );
    let dropped = t.trim_live(true);
    assert_eq!(dropped, 3);
    let groups = t.fold_groups();
    assert_eq!(groups.len(), 1, "the survivors' group is kept");
    assert_eq!(groups[0].start, call_idx - dropped);
}

/// A fold group entirely behind the drain is dropped, so no stale index
/// points into the evicted prefix.
#[test]
fn test_trim_drop_fold_group() {
    let mut lines = vec![TranscriptLine::User("go".into())];
    lines.push(bash_call("c1"));
    lines.push(bash_result("c1"));
    lines.extend(
        repeat_with(|| TranscriptLine::Agent("filler".into())).take(VIEWABLE_SCROLLBACK_CAP),
    );
    let mut t = Transcript::from(lines);
    assert_eq!(t.fold_groups().len(), 1, "group forms before the drain");
    let dropped = t.trim_live(true);
    assert_eq!(dropped, 3);
    assert!(t.fold_groups().is_empty(), "evicted group is dropped");
}

/// A group straddling the drain keeps its surviving tail: when the cut
/// lands inside the group, the start clamps to the new front instead of
/// underflowing.
#[test]
fn test_trim_clamp_straddled_group() {
    let mut lines = vec![TranscriptLine::User("go".into())];
    lines.push(bash_call("c1"));
    lines.push(bash_result("c1"));
    lines.push(bash_call("c2"));
    lines.push(bash_result("c2"));
    // Two consecutive bash pairs form one group [1, 5); the filler behind
    // them makes the total CAP + 3 so the drain cuts exactly at index 3.
    lines.extend(
        repeat_with(|| TranscriptLine::Agent("filler".into())).take(VIEWABLE_SCROLLBACK_CAP - 2),
    );
    let mut t = Transcript::from(lines);
    assert_eq!(t.fold_groups().len(), 1, "one group spans both pairs");
    assert_eq!(t.fold_groups()[0].start, 1);
    assert_eq!(t.fold_groups()[0].end, 5);

    let dropped = t.trim_live(true);
    assert_eq!(dropped, 3);

    let groups = t.fold_groups();
    assert_eq!(groups.len(), 1, "straddled group keeps its surviving tail");
    assert_eq!(groups[0].start, 0, "start clamps to the new front");
    assert_eq!(groups[0].end, 2, "end shifts to the surviving c2 pair");
}

fn user_frame(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

fn agent_frame(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

/// The resident byte total follows the frames through a push and a drain,
/// and the absolute coordinates survive: the window start advances past
/// the drained frames without renumbering the ones it kept.
#[test]
fn test_resident_bytes_track_drain() {
    let mut t = Transcript::default();
    for i in 0..5 {
        t.push_frame(user_frame(&format!("m{i}")));
    }
    let total = t.resident_bytes();
    assert!(total > 0, "a pushed frame counts toward the total");
    assert_eq!(t.abs_frame_count(), 5);

    let freed: u64 = t.frames()[..2]
        .iter()
        .map(|sf| sf.estimated_bytes() as u64)
        .sum();
    // A budget below the total but above what one frame holds drops the
    // oldest frames until the total fits.
    let dropped = t.drain_front_to_budget(total - freed);
    assert_eq!(dropped, 2, "the drain takes the frames the budget needs");
    assert_eq!(t.frame_window_start(), 2);
    assert_eq!(t.abs_frame_count(), 5, "draining does not renumber");
    assert_eq!(t.resident_bytes(), total - freed);
    assert_eq!(t.frame_count(), 3, "the drained frames are gone");
    assert!(
        matches!(&t.frames()[0].frame, TranscriptFrame::Session(SessionUpdate::UserMessageChunk(c)) if chunk_text(c) == "m2"),
        "the kept window opens at the first surviving frame"
    );
}

/// The drain takes any frame but the newest, so a log that holds no frame to
/// protect still reaches its budget. The newest frame stays even when it alone
/// exceeds the budget: a resident log drained empty has nothing left to
/// render.
#[test]
fn test_drain_without_boundary() {
    let mut t = Transcript::default();
    for i in 0..5 {
        t.push_frame(user_frame(&format!("m{i}")));
    }
    let dropped = t.drain_front_to_budget(1);
    assert_eq!(dropped, 4, "the drain takes every frame but the newest");
    assert_eq!(t.frame_count(), 1, "the newest frame stays resident");
    assert_eq!(t.abs_frame_count(), 5, "draining does not renumber");
    assert_eq!(
        t.drain_front_to_budget(1),
        0,
        "a lone frame above the budget is not drained away"
    );
}

/// The drain carries the turn its frames belonged to: a read that starts at
/// the new front opens inside that turn and cannot reach the frame that opened
/// it, so the state the drain left is the only record of it. A log whose front
/// is zero carries nothing, and the log itself holds the turn's opening.
#[test]
fn test_drain_carries_open_turn() {
    let mut t = Transcript::default();
    t.push_frame(user_frame("go"));
    t.push_frame(agent_frame("first answer"));
    t.push_frame(agent_frame("second answer"));
    assert!(
        t.front_checkpoint().is_none(),
        "a log nothing was drained from carries no state"
    );
    let freed: u64 = t.frames()[..2]
        .iter()
        .map(|sf| sf.estimated_bytes() as u64)
        .sum();
    assert_eq!(
        t.drain_front_to_budget(t.resident_bytes() - freed),
        2,
        "the budget takes the opening frame and the answer that followed it"
    );
    let state = t
        .front_checkpoint()
        .expect("the drained frames leave the turn's state behind");
    assert!(state.is_open(), "the turn they opened is still running");
    assert_eq!(
        state.opened_at(),
        Some(&RowId::local(0)),
        "the state names the frame that opened the turn"
    );
}

/// Rows read back from the session log lead the viewable lines, and the
/// fold cache follows them: the groups they bring sit at the front and
/// every group the resident rows had shifts down by their count. Dropping
/// the loaded rows shifts the survivors back up.
#[test]
fn test_disk_rows_shift_folds() {
    let mut lines = vec![TranscriptLine::User("go".into())];
    lines.push(bash_call("c1"));
    lines.push(bash_result("c1"));
    let mut t = Transcript::from(lines);
    assert_eq!(t.fold_groups()[0].start, 1, "the resident pair folds");

    let older = vec![bash_call("c0"), bash_result("c0")];
    t.prepend_disk_rows(NamedRows::from_lines(older), 4096, 7);
    assert_eq!(t.disk_row_count(), 2);
    assert_eq!(t.disk_front(), DiskFront::At(4096));
    assert_eq!(t.disk_rows_front(), 7);
    assert_eq!(t.fold_groups().len(), 2, "the loaded pair folds too");
    assert_eq!(t.fold_groups()[0].start, 0, "the loaded group leads");
    assert_eq!(t.fold_groups()[1].start, 3, "the resident group shifted");

    assert_eq!(t.clear_disk_rows(), 2);
    assert_eq!(t.disk_front(), DiskFront::Unloaded);
    assert_eq!(t.disk_row_count(), 0);
    assert_eq!(t.fold_groups().len(), 1);
    assert_eq!(t.fold_groups()[0].start, 1, "the resident group shifts up");
}

/// Releasing the rows read back from the session log counts their removal
/// once. The cap at the tail drops them from the visible list first, and
/// the fold cache that indexes that list must not count the same removal
/// again when the rows are released: every surviving group would sit a
/// loaded-row too high, over lines it does not cover.
#[test]
fn test_disk_rows_release_once() {
    let mut lines = vec![
        TranscriptLine::User("go".into()),
        bash_call("c1"),
        bash_result("c1"),
    ];
    while lines.len() <= VIEWABLE_SCROLLBACK_CAP {
        lines.push(TranscriptLine::User("pad".into()));
    }
    let mut t = Transcript::from(lines);
    t.prepend_disk_rows(
        NamedRows::from_lines(vec![TranscriptLine::User("older".into())]),
        4096,
        0,
    );

    t.trim_live(true);
    t.clear_disk_rows();

    assert_eq!(t.disk_row_count(), 0);
    let cached: Vec<(usize, usize)> = t.fold_groups().iter().map(|g| (g.start, g.end)).collect();
    let fresh: Vec<(usize, usize)> = compute_fold_groups(t.lines(), false)
        .iter()
        .map(|g| (g.start, g.end))
        .collect();
    assert_eq!(
        cached, fresh,
        "the fold cache indexes the list the release left"
    );
}

/// A payload edit through the closure door re-derives the byte total, so a
/// frame that grew cannot leave the counter under the true size and let a
/// later eviction hold more than the budget.
#[test]
fn test_frames_mut_recounts_bytes() {
    let mut t = Transcript::default();
    t.push_frame(user_frame("short"));
    let before = t.resident_bytes();
    t.with_frames_mut(|log| {
        *log = vec![user_frame(&"x".repeat(4096)).into()];
    });
    assert!(
        t.resident_bytes() > before + 4000,
        "the swapped frame's larger payload counts"
    );
}

fn disk_row(name: &str, text: &str) -> NamedRows {
    NamedRows::from_parts(
        vec![TranscriptLine::User(text.into())],
        vec![RowId::Durable {
            event: DurableEventId(name.into()),
            slot: 0,
        }],
    )
}

/// A row read back from the session log arrives with its own name and keeps
/// it at the front of the list, and the rows behind it keep theirs: a scroll
/// anchor held on a resident row must still find that row after the read
/// lands, wherever the list now stands.
#[test]
fn test_prepend_keeps_row_identity() {
    let mut t = Transcript::from(vec![
        TranscriptLine::User("go".into()),
        TranscriptLine::Agent("answer".into()),
    ]);
    t.prepend_disk_rows(disk_row("disk", "older"), 4096, 0);

    assert_eq!(t.row_ids().len(), t.len(), "every row keeps its name");
    assert!(
        matches!(t.lines()[0], TranscriptLine::User(ref s) if s == "older"),
        "the loaded row leads"
    );
    assert_eq!(
        t.row_ids()[0],
        RowId::Durable {
            event: DurableEventId("disk".into()),
            slot: 0
        },
        "the leading row carries the name the log gave it"
    );
    assert_eq!(
        t.row_ids()[2],
        RowId::local(1),
        "the last row keeps its name"
    );

    t.clear_disk_rows();
    assert_eq!(t.row_ids().len(), t.len());
    assert_eq!(
        t.row_ids(),
        [RowId::local(0), RowId::local(1)],
        "releasing the loaded rows takes their names with them"
    );
}

/// The cap cuts the front of the list, so the names it takes belong to the
/// rows it drops: a name left behind would point one row up for every row the
/// cut removed.
#[test]
fn test_trim_keeps_row_identity() {
    let lines: Vec<TranscriptLine> = (0..VIEWABLE_SCROLLBACK_CAP + 3)
        .map(|i| TranscriptLine::Agent(format!("row {i}")))
        .collect();
    let mut t = Transcript::from(lines);
    let dropped = t.trim_live(true);
    assert_eq!(dropped, 3);

    assert_eq!(t.row_ids().len(), t.len());
    assert_eq!(t.len(), VIEWABLE_SCROLLBACK_CAP);
    assert_eq!(
        t.row_ids()[0],
        RowId::local(dropped as u64),
        "the new front row carries the name it was dropped onto"
    );
    assert!(
        matches!(t.lines()[0], TranscriptLine::Agent(ref s) if s == "row 3"),
        "the surviving row is the one that name belongs to"
    );
}

/// Swapping a line's payload in place is the one write a rebuild does not
/// re-derive, so it moves no name: the row that stands for a child keeps the
/// identity the fold state and any anchor hold for it.
#[test]
fn test_edit_keeps_row_identity() {
    let mut t = Transcript::from(vec![
        TranscriptLine::User("go".into()),
        TranscriptLine::Agent("fetched".into()),
    ]);
    let names = t.row_ids().to_vec();
    t.edit_line(1, |line| *line = TranscriptLine::Agent("swapped".into()));

    assert!(
        matches!(t.lines()[1], TranscriptLine::Agent(ref s) if s == "swapped"),
        "the edit lands on the row it names"
    );
    assert_eq!(t.row_ids(), names, "an edit moves no name");
}

/// A row the frontend raises with no frame behind it still enters the list
/// with one name of its own, and a rebuild that assembles the list from the
/// frames and blocks drops it rather than leaving it behind without one.
#[test]
fn test_unframed_rows_named_separately() {
    let mut t = Transcript::default();
    t.push(TranscriptLine::System("first".into()));
    t.push(TranscriptLine::System("second".into()));

    assert_eq!(t.len(), 2);
    assert_eq!(t.row_ids().len(), 2);
    assert_ne!(
        t.row_ids()[0],
        t.row_ids()[1],
        "two raised rows hold two names"
    );

    let (rows, _) = t.visible_rows(0, 0, 0);
    t.replace_rows(rows);
    assert!(
        t.is_empty(),
        "the rebuild drops raised rows along with their names"
    );
}
