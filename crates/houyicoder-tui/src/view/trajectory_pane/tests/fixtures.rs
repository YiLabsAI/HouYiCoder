//! The shape of the pane's view types, for the tests that read them.
//!
//! One owner for the fields a test does not care about: a test starts from a
//! fixture and sets the part it is about, so a new field on a view type is
//! answered here once instead of in every test that builds one.

use super::super::*;

/// A turn row with the given number and prompt, and nothing measured. A test
/// sets the fields it is about on the value it gets back.
pub(super) fn turn(n: usize, user_input: &str) -> TrajectoryTurn {
    TrajectoryTurn {
        n,
        key: TrajectoryTurnKey::from_opening_event(&format!("t{n}")),
        boundary_before: Vec::new(),
        title: user_input.into(),
        tokens_in: None,
        tokens_out: None,
        cache_read: None,
        cache_write: None,
        models: Vec::new(),
        efforts: Vec::new(),
        reasoning_tokens: None,
        tool_count: 0,
        tool_fail: 0,
        retries: 0,
        duration_ms: 0,
        success: true,
    }
}

/// A record of the given kind, with the tool name the pane shows and a measured
/// span, for tests that read a row or a detail.
pub(super) fn record_of(kind: TrajectoryRecordKind, output: Option<&str>) -> TrajectoryRecord {
    TrajectoryRecord {
        kind,
        name: Some("bash".into()),
        ordinal: 0,
        summary: "preview".into(),
        start_ms: 0,
        duration_ms: 10,
        outcome: RecordOutcome::Ok,
        thinking: None,
        input: None,
        output: output.map(Into::into),
        usage: None,
        timing: None,
        retries: 0,
    }
}

/// A settled view over the given rows, with nothing else measured.
pub(super) fn view(rows: Vec<TrajectoryRow>) -> TrajectoryView {
    TrajectoryView {
        state: TrajectoryViewState::Ready,
        skipped_records: 0,
        models_used: 1,
        tool_calls: 0,
        session_id: "s".into(),
        model: "m".into(),
        total_turns: rows.len(),
        tokens_in: None,
        tokens_out: None,
        failures: 0,
        duration_secs: 0,
        cache_read: None,
        timing: SessionTiming::default(),
        hidden_turns: 0,
        newer_hidden: 0,
        history_generation: 0,
        subagent_usage: None,
        rows,
    }
}

/// A settled view over the given turns, each as its own row.
pub(super) fn view_of(turns: Vec<TrajectoryTurn>) -> TrajectoryView {
    view(turns.into_iter().map(TrajectoryRow::Turn).collect())
}

/// A detail holding the given records, ready to render.
pub(super) fn detail_of_all(records: Vec<TrajectoryRecord>) -> TrajectoryDetailView {
    TrajectoryDetailView {
        state: TrajectoryDetailState::Ready,
        turn: Some(turn(1, "ask")),
        records,
        truncated: false,
    }
}

/// A detail holding one record.
pub(super) fn detail_of(record: TrajectoryRecord) -> TrajectoryDetailView {
    detail_of_all(vec![record])
}

/// A window of turn rows numbered first..=last, over a session of total turns.
pub(super) fn window_view(
    first: usize,
    last: usize,
    total: usize,
    generation: u64,
) -> TrajectoryView {
    let mut v = view_of(
        (first..=last)
            .map(|n| turn(n, &format!("prompt {n}")))
            .collect(),
    );
    v.total_turns = total;
    v.hidden_turns = first.saturating_sub(1);
    v.newer_hidden = total.saturating_sub(last);
    v.history_generation = generation;
    v
}

/// A rendered line group's text, spans joined.
pub(super) fn lines_text(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect()
}

/// The turn number the pane is on, for a test that is about the number rather
/// than about the history it was read in.
pub(super) fn selected_number(app: &crate::state::App) -> Option<usize> {
    app.trajectory.selection().map(|s| s.number)
}
