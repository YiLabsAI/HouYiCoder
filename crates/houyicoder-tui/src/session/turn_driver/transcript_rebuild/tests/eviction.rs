use super::*;
use crate::records::TURN_CONTENT_OUT_OF_VIEW;
use crate::scroll::VIEWABLE_SCROLLBACK_CAP;
use crate::transcript::FrontendRow;

/// Trim runs only at the tail. A scrolled-back reader pins a fixed viewport,
/// so draining the oldest lines would shift the content under it; a scroll-up
/// session's loaded older frames must also survive the next rebuild. This
/// floods the live transcript past the cap while scrolled back and asserts the
/// lines accumulate rather than draining.
#[test]
fn test_trim_skips_scrollaway() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    // Follow the tail: direct pushes trim down to the cap.
    for _ in 0..(VIEWABLE_SCROLLBACK_CAP + 5) {
        app.push_transcript_line(TranscriptLine::Agent("tail line".into()));
    }
    assert_eq!(
        app.transcript().len(),
        VIEWABLE_SCROLLBACK_CAP,
        "follow-tail direct push trims to the cap"
    );
    // Scroll away from the tail, then keep pushing: trim must not drain a
    // scrolled-back reader, so the lines accumulate past the cap.
    draw_transcript(&mut app);
    app.park_scroll_at(0);
    let extra = 50;
    for _ in 0..extra {
        app.push_transcript_line(TranscriptLine::Agent("scrolled-back line".into()));
    }
    assert_eq!(
        app.transcript().len(),
        VIEWABLE_SCROLLBACK_CAP + extra,
        "trim skips while scrolled back, so lines accumulate"
    );
}

/// When trim drains at the tail, the turn boundary shifts with it so the next
/// tail rebuild's stable-prefix slice stays aligned. Before the fix the cap
/// drained underneath an unchanged boundary, leaving it pointing past the
/// prefix the next rebuild would reuse.
#[test]
fn test_trim_shifts_boundary() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    // A turn boundary with a real prefix: 100 user messages, then an agent
    // frame, rebuild sets line_index to the prefix length (100).
    for i in 0..100 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("prefix {i}")));
    }
    app.transcript_mut().push_frame(agent_msg("turn body"));
    app.rebuild_transcript();
    let boundary_before = app.transcript_mut().current_turn_mut().line_index;
    assert!(boundary_before > 0, "boundary names the prefix length");
    // Flood past the cap at the tail: trim drains, and the boundary shifts
    // down by the dropped count (saturating at zero).
    let over = VIEWABLE_SCROLLBACK_CAP + 10;
    for _ in 0..over {
        app.push_transcript_line(TranscriptLine::Agent("flood".into()));
    }
    assert!(
        app.transcript_mut().current_turn_mut().line_index <= app.transcript().len(),
        "boundary stays within the post-trim transcript length"
    );
    assert!(
        app.transcript_mut().current_turn_mut().line_index < boundary_before,
        "boundary shifted down as the prefix was trimmed"
    );
}

/// The scroll-up white-load: the user scrolled back and loaded older frames;
/// a new frame arriving triggers a rebuild whose exit must not drain the rows
/// just loaded. Before the fix the rebuild exit capped unconditionally and
/// drained the prepended history, so scrolling up was defeated the moment new
/// content arrived.
#[test]
fn test_scrollback_survives_frame() {
    // Enough frames that repeated scroll-up prepends push the transcript
    // past the viewable cap: the frame window keeps the newest rows, and the
    // prepended older rows must push the total past the cap.
    let frame_count = VIEWABLE_SCROLLBACK_CAP + 600;
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    for i in 0..frame_count {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    // Scroll away and load older frames until the history boundary reaches the
    // resident front. A batch lands above the parked row, so the viewport stands
    // deeper afterwards and the reader steps back to the top between loads --
    // the gesture that asks for the next batch. Bounded: a batch reaches
    // PREPEND_BATCH frames further back each round, so a loop outliving this
    // many rounds is one that stopped reaching back, which fails rather than
    // spins.
    draw_transcript(&mut app);
    let front = app.transcript().frame_window_start();
    let mut rounds = 0;
    while app.sessions.current.loaded_from_frame.get() > front {
        rounds += 1;
        assert!(rounds < 60, "the loads reach the resident front");
        app.park_scroll_at(0);
        app.load_older_frames();
        pump_history_read_blocking(&mut app, Duration::from_secs(5));
    }
    assert!(
        app.transcript().len() > VIEWABLE_SCROLLBACK_CAP,
        "prepended history pushes the transcript past the cap"
    );
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("msg 0"))),
        "oldest loaded row present before the new frame"
    );
    // A new frontend row arrives while scrolled back. The rebuild exit must
    // not trim a scrolled-back reader, so the prepended history survives.
    app.transcript_mut()
        .push_frame(TranscriptFrame::Frontend(FrontendRow::System("new".into())));
    app.rebuild_transcript();
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.contains("msg 0"))),
        "scroll-up history survives the frame arrival"
    );
}

#[test]
fn test_fold_cache_tracks_recompute() {
    let mut app = fresh_app();
    app.start_run_for_test(0);
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "bash"));
    pump(&mut app, tool_result("c1"));
    pump(&mut app, tool_call("c2", "bash"));
    pump(&mut app, tool_result("c2"));
    assert_eq!(fold_signature(&app), recompute_fold_signature(&app));

    // More pairs in the same turn exercise the incremental tail recompute
    // (the frozen prefix is reused, only the tail rescans).
    pump(&mut app, tool_call("c3", "bash"));
    pump(&mut app, tool_result("c3"));
    assert_eq!(fold_signature(&app), recompute_fold_signature(&app));

    // A new turn moves the boundary, re-derives the prefix, and flips the
    // prior turn's group from active to complete.
    pump(&mut app, user_msg("again"));
    pump(&mut app, tool_call("d1", "bash"));
    pump(&mut app, tool_result("d1"));
    assert_eq!(fold_signature(&app), recompute_fold_signature(&app));

    let sig = fold_signature(&app);
    assert_eq!(sig.len(), 2, "one group per turn");
    assert!(!sig[0].3, "prior turn group completes");
    assert!(sig[1].3, "active turn group stays open");
}

/// A call id reused across turns must not collide in the fold cache. The
/// incremental tail path seeds its per-call-id ordinal from the retained
/// prefix, so turn two's re-emitted c1 continues to c1#1 instead of
/// colliding with turn one's c1#0.
#[test]
fn test_fold_reused_call_id() {
    let mut app = fresh_app();
    app.start_run_for_test(0);
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("c1", "bash"));
    pump(&mut app, tool_result("c1"));
    pump(&mut app, user_msg("again"));
    pump(&mut app, tool_call("c1", "bash"));
    pump(&mut app, tool_result("c1"));

    let sig = fold_signature(&app);
    assert_eq!(sig.len(), 2, "one group per turn");
    assert_ne!(sig[0].2, sig[1].2, "reused call id must not collide");
    assert_eq!(sig, recompute_fold_signature(&app));
}

/// The active flag responds to a pause without a rebuild: begin_waiting
/// collapses the active group and end_waiting reopens it, both through the
/// refresh the run-state transitions trigger.
#[test]
fn test_fold_active_tracks_waiting() {
    let mut app = fresh_app();
    app.start_run_for_test(0);
    pump(&mut app, user_msg("go"));
    pump(&mut app, tool_call("w1", "bash"));
    pump(&mut app, tool_result("w1"));
    assert!(fold_signature(&app)[0].3, "running group is active");

    app.sessions.current.run.begin_waiting();
    app.refresh_fold_active();
    assert!(!fold_signature(&app)[0].3, "waiting group collapses");

    app.sessions.current.run.end_waiting();
    app.refresh_fold_active();
    assert!(fold_signature(&app)[0].3, "resumed group reopens");
}

/// The resident frame log is bounded by bytes: a session long enough to exceed
/// the budget drains its oldest frames instead of holding every frame forever,
/// and the newest rows still render.
#[test]
fn test_evicts_under_byte_budget() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(100 * 1024);
    let text = "x".repeat(100);
    for i in 0..1000 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("{i} {text}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript().frame_window_start() > 0,
        "frames past the budget are drained"
    );
    assert!(
        app.transcript().resident_bytes() <= app.transcript().resident_byte_budget() as u64,
        "the drain reaches the budget when the window itself fits under it"
    );
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.starts_with("999 "))),
        "the newest row renders"
    );
}

/// No block names a frame below the resident front, and no rendered row
/// derives from one: the frames the drain reclaims belong to turns the window
/// has moved past, so the settled view holds only what the resident window
/// covers.
#[test]
fn test_eviction_stops_at_front() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(1024);
    let text = "x".repeat(100);
    for turn in 0..4 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("turn {turn}")));
        for i in 0..300 {
            app.transcript_mut()
                .push_frame(agent_msg(&format!("{turn}.{i} {text}")));
        }
    }
    app.rebuild_transcript();
    assert!(
        app.transcript().frame_window_start() > 0,
        "the budget drains the frames of the closed turns"
    );
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::Agent(t) if t.starts_with("3.299 "))),
        "the tail row renders after the drain"
    );
    // The pass that drains still renders the frames it reclaimed; the next
    // rebuild derives the view from the resident window alone.
    app.rebuild_transcript();
    for b in app.transcript().blocks().blocks() {
        assert!(
            b.frame_range.start >= app.transcript().frame_window_start(),
            "block {:?} reaches below the resident front",
            b.frame_range
        );
    }
    assert!(
        !app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "turn 3")),
        "the settled view derives no row from a drained frame"
    );
}

/// A rebuild that drains keeps the verdicts of the frames it reclaims: the
/// audit scan runs before the drain, so a frame that leaves the resident window
/// in the same pass is already folded into the cache and stays there.
#[test]
fn test_verdict_survives_drain() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(1024);
    app.transcript_mut()
        .push_frame(TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextPermissionDecision,
            json!({
                "tool": "bash",
                "verdict": "allow",
                "scope": "once",
                "callId": "c1",
            }),
        )));
    let text = "x".repeat(100);
    for turn in 0..4 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("turn {turn}")));
        for i in 0..300 {
            app.transcript_mut()
                .push_frame(agent_msg(&format!("{turn}.{i} {text}")));
        }
    }
    app.rebuild_transcript();
    assert!(
        app.transcript().frame_window_start() > 0,
        "the rebuild drains the frames below the active turn"
    );
    assert_eq!(
        app.sessions
            .current
            .capabilities
            .permission
            .verdict_log_cache
            .len(),
        1,
        "the drained frame's verdict is in the audit cache"
    );
    assert_eq!(
        app.sessions
            .current
            .capabilities
            .permission
            .verdict_log_cache[0]
            .call_id,
        "c1"
    );
}

/// A log that holds no user frame still reaches the budget: with no turn
/// boundary to protect, the drain takes the oldest frames down to the newest.
/// The window a resumed session reads can start mid-turn, so the bound cannot
/// depend on a user frame being resident.
#[test]
fn test_drain_without_user_frame() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(8 * 1024);
    let text = "x".repeat(1000);
    for i in 0..40 {
        app.transcript_mut()
            .push_frame(agent_msg(&format!("{i} {text}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript().frame_window_start() > 0,
        "the budget drains a log whose window holds no user frame"
    );
    assert!(
        app.transcript().resident_bytes() <= app.transcript().resident_byte_budget() as u64,
        "the drain reaches the budget: {} > {}",
        app.transcript().resident_bytes(),
        app.transcript().resident_byte_budget()
    );
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::Agent(t) if t.starts_with("39 "))),
        "the newest row renders"
    );
}

/// Scrollback loading stops at the resident front: a batch whose frames were
/// drained cannot be re-derived, so the loaded boundary holds at the front
/// rather than naming frames the window cannot read.
#[test]
fn test_scrollback_stops_at_front() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(512 * 1024);
    let text = "x".repeat(400);
    for i in 0..2000 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("{i} {text}")));
    }
    app.rebuild_transcript();
    let base = app.transcript().frame_window_start();
    // The clamp binds only when the load reaches past the front while the frame
    // cap still sits above it, which is where a load asks for drained frames.
    let cap = 2000 - 500;
    assert!(
        base > 0 && base + 1 < cap,
        "the drain leaves a loadable range below the frame cap: front={base}"
    );
    draw_transcript(&mut app);
    app.park_scroll_at(0);
    app.sessions.current.loaded_from_frame.set(base + 1);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert_eq!(
        app.sessions.current.loaded_from_frame.get(),
        base,
        "the loaded boundary stops at the resident front"
    );
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t.starts_with("1999 "))),
        "the tail survives the load"
    );
}

/// The active turn is bounded like every other: a turn whose own frames exceed
/// the budget drains its oldest frames, and the row that opens the view says
/// where the part the window can no longer show went.
#[test]
fn test_drain_bounds_active_turn() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(1024);
    let text = "x".repeat(200);
    app.transcript_mut().push_frame(user_msg("opening"));
    for i in 0..100 {
        app.transcript_mut()
            .push_frame(agent_msg(&format!("{i} {text}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript().frame_window_start() > 0,
        "the drain bounds the active turn itself"
    );
    assert!(
        app.transcript().resident_bytes() <= app.transcript().resident_byte_budget() as u64,
        "the drain reaches the budget"
    );
    // The pass that drains still renders the frames it reclaimed; the next
    // rebuild derives the view from the resident window alone.
    app.rebuild_transcript();
    assert!(
        matches!(
            app.transcript().lines().first(),
            Some(TranscriptLine::System(text)) if text.as_str() == TURN_CONTENT_OUT_OF_VIEW
        ),
        "the view opens on the row standing for the drained beginning: {:?}",
        app.transcript().lines().first()
    );
    draw_transcript(&mut app);
    assert!(
        (0..20).any(|row| transcript_row(&app, row).contains(TURN_CONTENT_OUT_OF_VIEW)),
        "the pane tells the reader the earlier content of the turn is on disk"
    );
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::Agent(t) if t.starts_with("99 "))),
        "the newest row still renders"
    );
}

/// The drain carries what the frames it reclaims said about their turn: the
/// row that closes the turn still reports the reasoning and the calls that
/// left the window, so the summary describes the turn rather than the part of
/// it the window happens to hold.
#[test]
fn test_drain_keeps_turn_summary() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(1024);
    app.transcript_mut().push_frame(user_msg("opening"));
    app.transcript_mut()
        .push_frame(thought_msg(&"t".repeat(300)));
    app.transcript_mut().push_frame(tool_call("c1", "bash"));
    app.transcript_mut().push_frame(tool_result("c1"));
    let text = "x".repeat(200);
    for i in 0..40 {
        app.transcript_mut()
            .push_frame(agent_msg(&format!("{i} {text}")));
    }
    app.transcript_mut().push_frame(user_msg("next turn"));
    app.rebuild_transcript();
    assert!(
        app.transcript().frame_window_start() > 4,
        "the drain takes the frames the turn's facts came from: {}",
        app.transcript().frame_window_start()
    );
    app.rebuild_transcript();
    let (reasoning, tools) = app
        .transcript()
        .lines()
        .iter()
        .find_map(|line| match line {
            TranscriptLine::ThoughtFor {
                reasoning,
                tool_summary,
                ..
            } => Some((reasoning.clone(), tool_summary.clone())),
            _ => None,
        })
        .expect("the drained turn keeps its summary row");
    assert!(
        reasoning
            .as_deref()
            .is_some_and(|text| text.contains("300 characters")),
        "the summary counts the reasoning the drain took: {reasoning:?}"
    );
    assert_eq!(
        tools.as_deref(),
        Some("ran 1 bash"),
        "the summary counts the call the drain took"
    );
}

/// A result whose own call frame left the window still names the tool it
/// answers: the call is what tells a result apart from a line of its own, and
/// the state the drain carried is the only place it is still named.
#[test]
fn test_result_names_drained_call() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(1024);
    app.transcript_mut().push_frame(user_msg("opening"));
    app.transcript_mut().push_frame(tool_call("c1", "bash"));
    let text = "x".repeat(200);
    for i in 0..40 {
        app.transcript_mut()
            .push_frame(agent_msg(&format!("{i} {text}")));
    }
    app.transcript_mut().push_frame(tool_result("c1"));
    app.rebuild_transcript();
    assert!(
        app.transcript().frame_window_start() > 1,
        "the drain takes the call's own frame: {}",
        app.transcript().frame_window_start()
    );
    app.rebuild_transcript();
    let (name, tool) = app
        .transcript()
        .lines()
        .iter()
        .find_map(|line| match line {
            TranscriptLine::Tool {
                call_id,
                name,
                tool,
                ..
            } if call_id == "c1" => Some((name.clone(), tool.clone())),
            _ => None,
        })
        .expect("the result renders under the row standing for its call");
    assert_eq!(
        tool, "bash",
        "the row names the tool the drained call was for, not a bare call"
    );
    assert_ne!(name, "call", "the row is not the unnamed form");
}

/// A scrollback load lowers the window front below the frame cap. The byte
/// budget still drains afterwards, so a session that reached for older frames
/// is not exempt from the bound.
#[test]
fn test_load_boundary_keeps_draining() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(128 * 1024);
    let text = "x".repeat(100);
    for i in 0..600 {
        pump(&mut app, user_msg(&format!("{i} {text}")));
    }
    assert_eq!(
        app.transcript().frame_window_start(),
        0,
        "the budget holds the frames before a load"
    );
    draw_transcript(&mut app);
    app.park_scroll_at(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert!(
        app.sessions.current.loaded_from_frame.get() < 100,
        "the load reaches below the frame cap: {}",
        app.sessions.current.loaded_from_frame.get()
    );
    for i in 600..1000 {
        pump(&mut app, user_msg(&format!("{i} {text}")));
    }
    let base = app.transcript().frame_window_start();
    let bytes = app.transcript().resident_bytes();
    let budget = app.transcript().resident_byte_budget() as u64;
    assert!(base > 0, "the budget drains after a load: base={base}");
    assert!(
        bytes <= budget,
        "resident bytes stay under the budget: {bytes} > {budget}"
    );
}

/// A window whose front lands on the record that closed a turn opens inside
/// that turn: the frames below it left the turn running, and the record the
/// window starts on closes the turn rather than opening anything, so the rows
/// the window derives belong to a turn it holds no beginning of. The front
/// needs no user frame below it for this — a window that starts on a closing
/// record is the shape a resumed session reads.
#[test]
fn test_drain_marks_closed_turn() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    let text = "x".repeat(200);
    app.transcript_mut().push_frame(user_msg("opening"));
    for i in 0..40 {
        app.transcript_mut()
            .push_frame(agent_msg(&format!("{i} {text}")));
    }
    app.transcript_mut()
        .push_frame(TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextRunCompleted,
            json!({ "ms": 5 }),
        )));
    let record = app.sessions.current.transcript.frames()[41].estimated_bytes() as u64;
    assert_eq!(
        app.sessions
            .current
            .transcript
            .drain_front_to_budget(record),
        41,
        "the drain takes every frame above the closing record"
    );
    app.rebuild_transcript();
    assert!(
        matches!(
            app.sessions.current.transcript.frames().first().map(|f| f.as_ref()),
            Some(TranscriptFrame::Acpx(n)) if n.method == AcpxMethod::ContextRunCompleted
        ),
        "the window front is the closing record"
    );
    assert!(
        matches!(
            app.transcript().lines().first(),
            Some(TranscriptLine::System(text)) if text.as_str() == TURN_CONTENT_OUT_OF_VIEW
        ),
        "the view opens on the row standing for the drained beginning: {:?}",
        app.transcript().lines().first()
    );
}

/// A drain front landing on the message that opens the next turn draws no row
/// standing for content that left memory: that message is where the turn
/// below it ends, and the rows the window derives begin at the turn it opens.
/// The frames below left a turn running, so a row drawn on the turn state
/// alone would claim the window opens inside a turn it holds no part of.
#[test]
fn test_opener_leads_without_row() {
    let mut app = fresh_app();
    let text = "x".repeat(200);
    app.transcript_mut().push_frame(user_msg("first"));
    for i in 0..40 {
        app.transcript_mut()
            .push_frame(agent_msg(&format!("{i} {text}")));
    }
    app.transcript_mut().push_frame(user_msg("second"));
    for i in 0..20 {
        app.transcript_mut()
            .push_frame(agent_msg(&format!("after {i} {text}")));
    }
    let kept: u64 = app.sessions.current.transcript.frames()[..41]
        .iter()
        .map(|frame| frame.estimated_bytes() as u64)
        .sum();
    let budget = app.transcript().resident_bytes() - kept;
    assert_eq!(
        app.sessions
            .current
            .transcript
            .drain_front_to_budget(budget),
        41,
        "the drain stops on the frame that opens the next turn"
    );
    app.rebuild_transcript();
    assert!(
        matches!(app.transcript().first(), Some(TranscriptLine::User(t)) if t == "second"),
        "the view opens on the row that opens the turn: {:?}",
        app.transcript().lines().first()
    );
    assert!(
        !app.transcript().iter().any(
            |l| matches!(l, TranscriptLine::System(t) if t.as_str() == TURN_CONTENT_OUT_OF_VIEW)
        ),
        "no row claims content left memory above a row that opens a turn"
    );
}

/// A window the drain has taken no frame from holds every frame below its
/// front, so the row standing for content that left memory is not drawn even
/// when the frame cap moved that front: the cap keeps every frame resident,
/// and the rows above the window come back by scrolling. The drain's side of
/// the condition is test_drain_bounds_active_turn, and the frame that opens a
/// turn is test_opener_leads_without_row.
#[test]
fn test_drain_keeps_turn_start() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    for i in 0..710 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(
        matches!(app.transcript().first(), Some(TranscriptLine::User(t)) if t == "msg 210"),
        "the view opens on the row that opens the turn: {:?}",
        app.transcript().lines().first()
    );
    assert!(
        !app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "msg 209")),
        "the frame cap moved the window past the frames below its front"
    );
    assert!(
        !app.transcript().iter().any(
            |l| matches!(l, TranscriptLine::System(t) if t.as_str() == TURN_CONTENT_OUT_OF_VIEW)
        ),
        "no row claims the window cut a turn it holds whole"
    );
}

/// The frame cap alone leaves every frame in memory, so a window cut to the
/// cap draws no row standing in for content that is still there: the part of
/// the turn above the window comes back by scrolling, which is the frame
/// cap's own path, and the row would claim it left memory. The turn below the
/// cap front never closed, so the state read there reports it running, and
/// only the drain makes that state mean something left memory.
#[test]
fn test_cap_keeps_frames_resident() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().push_frame(user_msg("opening"));
    for i in 0..700 {
        app.transcript_mut().push_frame(agent_msg(&format!("{i}")));
    }
    app.transcript_mut().push_frame(user_msg("next"));
    app.rebuild_transcript();
    assert_eq!(
        app.transcript().frame_window_start(),
        0,
        "the budget holds every frame, so nothing left memory"
    );
    assert!(
        matches!(app.transcript().first(), Some(TranscriptLine::Agent(t)) if t == "201"),
        "the cap cut the window inside the turn below it: {:?}",
        app.transcript().lines().first()
    );
    assert!(
        !app.transcript().iter().any(
            |l| matches!(l, TranscriptLine::System(t) if t.as_str() == TURN_CONTENT_OUT_OF_VIEW)
        ),
        "no row claims content left memory when it is still resident"
    );
}

/// The row standing for a cut turn leaves the view once the rows it stands
/// for are read back: the window still opens inside the turn, but its earlier
/// content sits above on view, and the row would say that content was gone.
/// The cut turn here is the frozen prefix, which a pass reuses while nothing
/// about its frames changed: whether the row leads it is part of that decision.
#[test]
fn test_loaded_rows_drop_marker() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    let text = "x".repeat(200);
    app.transcript_mut().push_frame(user_msg("opening"));
    for i in 0..120 {
        app.transcript_mut()
            .push_frame(agent_msg(&format!("{i} {text}")));
    }
    app.transcript_mut().push_frame(user_msg("second"));
    for i in 0..20 {
        app.transcript_mut()
            .push_frame(agent_msg(&format!("after {i} {text}")));
    }
    // The log's rows: what a read of the whole log derives, which is the view
    // before any drain took a frame from it.
    app.transcript_mut().set_resident_byte_budget(1 << 20);
    app.rebuild_transcript();
    let mut log_rows: Vec<TranscriptLine> = (0..5)
        .map(|i| TranscriptLine::User(format!("older {i}")))
        .collect();
    log_rows.extend(app.transcript().iter().cloned());
    app.transcript_mut().set_resident_byte_budget(12 * 1024);
    app.rebuild_transcript();
    app.rebuild_transcript();
    assert!(
        matches!(
            app.transcript().lines().first(),
            Some(TranscriptLine::System(t)) if t.as_str() == TURN_CONTENT_OUT_OF_VIEW
        ),
        "the view opens on the row standing for the cut turn: {:?}",
        app.transcript().lines().first()
    );
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(log_rows, 100, 4096)]));

    draw_transcript(&mut app);
    app.park_scroll_at(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert!(
        app.transcript().disk_row_count() > 0,
        "the older rows landed above the window"
    );
    assert!(
        !app.transcript().iter().any(
            |l| matches!(l, TranscriptLine::System(t) if t.as_str() == TURN_CONTENT_OUT_OF_VIEW)
        ),
        "the row gives way once the content it stood for is on view: {:?}",
        &app.transcript().lines()[..8]
    );
}

/// A checklist whose todo-write frame was drained keeps its items: the cursor
/// is an absolute frame index, so eviction neither resets the accumulator nor
/// makes it re-read a frame that is gone.
#[test]
fn test_todo_survives_eviction() {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(1024);
    let text = "x".repeat(100);
    pump(
        &mut app,
        todo_write_frame("t1", &[("ship the window", "in_progress")]),
    );
    for i in 0..900 {
        pump(&mut app, user_msg(&format!("{i} {text}")));
    }
    assert!(
        app.transcript().frame_window_start() > 0,
        "the todo-write frame was drained"
    );
    assert!(
        app.sessions
            .current
            .todos
            .items
            .iter()
            .any(|t| t.content == "ship the window"),
        "the drained frame's checklist survives"
    );
    pump(&mut app, user_msg("tail"));
    assert!(
        app.sessions
            .current
            .todos
            .items
            .iter()
            .any(|t| t.content == "ship the window"),
        "the checklist survives the rebuild that follows"
    );
}
