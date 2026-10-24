use super::*;
use houyicoder_protocol::frontend::agents::AgentPrompt;

fn fleet_entry(agent_id: &str, completed: Option<&str>) -> FleetEntry {
    FleetEntry {
        agent_id: agent_id.into(),
        subagent_type: "explore".into(),
        turn: 1,
        tokens: 10,
        tool_uses: 0,
        last_activity: None,
        completed: completed.map(str::to_string),
        completed_at: None,
        started_at: None,
    }
}

fn delegation(child_sid: &str) -> DelegationRow {
    DelegationRow {
        child_sid: child_sid.into(),
        subagent_type: "explore".into(),
        summary: format!("summary of {child_sid}"),
        prompt: String::new(),
        loaded: true,
    }
}

fn agent_entry(subagent_type: &str) -> AgentEntry {
    AgentEntry {
        subagent_type: subagent_type.into(),
        when_to_use: "when it helps".into(),
        tools: None,
        disallowed_tools: Vec::new(),
        model: None,
        effort: None,
        system_prompt: AgentPrompt::InheritParent,
    }
}

/// The three sources list in one order: running children, then returned
/// delegations newest first, then the registered types.
#[test]
fn test_three_sections_in_order() {
    let fleet = [fleet_entry("c1", None), fleet_entry("c3", None)];
    let completed = [delegation("c1"), delegation("c2")];
    let types = [agent_entry("explore")];
    let rows = agent_rows(&fleet, &completed, Some(&types));
    let sections: Vec<&str> = rows.iter().map(AgentRow::section).collect();
    assert_eq!(
        sections,
        ["Running", "Completed", "Completed", "Available types"]
    );
    let AgentRow::Running(first) = &rows[0] else {
        panic!("a child still out is listed first");
    };
    assert_eq!(first.agent_id, "c3");
    let AgentRow::Completed(next) = &rows[1] else {
        panic!("the second row is a returned delegation");
    };
    assert_eq!(next.child_sid, "c2", "the newest delegation leads");
}

/// A child that has returned belongs to COMPLETED alone: listing it as running
/// too would let one delegation answer two Enter presses.
#[test]
fn test_completed_child_leaves_running() {
    let fleet = [fleet_entry("c1", Some("done")), fleet_entry("c2", None)];
    let rows = agent_rows(&fleet, &[], None);
    let running: Vec<&str> = rows
        .iter()
        .filter_map(|row| match row {
            AgentRow::Running(entry) => Some(entry.agent_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(running, ["c2"]);
}

/// A child whose returned delegation has already landed is listed once, even
/// while the fleet's own completion mark is still on its way: the two sources
/// settle independently, so the live list must read the returned one to know
/// the child is done.
#[test]
fn test_returned_child_listed_once() {
    let fleet = [fleet_entry("c1", None), fleet_entry("c2", None)];
    let completed = [delegation("c1")];
    let rows = agent_rows(&fleet, &completed, None);
    let running: Vec<&str> = rows
        .iter()
        .filter_map(|row| match row {
            AgentRow::Running(entry) => Some(entry.agent_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        running,
        ["c2"],
        "the child whose delegation returned is not also still running"
    );
    let listed: Vec<&str> = rows.iter().map(AgentRow::subagent_type).collect();
    assert_eq!(
        listed.len(),
        2,
        "the child is listed exactly once: {listed:?}"
    );
}

/// Only the newest returned delegations are listed. What the pane drops stays
/// in the transcript, so the list is a window on the recent ones rather than
/// the whole history.
#[test]
fn test_completed_keeps_newest_ten() {
    let completed: Vec<DelegationRow> = (0..12).map(|i| delegation(&format!("c{i}"))).collect();
    let rows = agent_rows(&[], &completed, None);
    assert_eq!(rows.len(), COMPLETED_LIMIT);
    let AgentRow::Completed(newest) = &rows[0] else {
        panic!("the first row is a returned delegation");
    };
    assert_eq!(newest.child_sid, "c11", "the newest is first");
    let AgentRow::Completed(oldest) = &rows[rows.len() - 1] else {
        panic!("the last row is a returned delegation");
    };
    assert_eq!(
        oldest.child_sid, "c2",
        "the oldest listed is the eleventh back"
    );
}

/// A reply that has not landed contributes no rows: the pane lists what it
/// has, and an unknown type list is not an empty one.
#[test]
fn test_absent_types_no_rows() {
    let types = [agent_entry("explore"), agent_entry("general-purpose")];
    assert!(agent_rows(&[], &[], None).is_empty());
    assert!(agent_rows(&[], &[], Some(&[])).is_empty());
    assert_eq!(agent_rows(&[], &[], Some(&types)).len(), 2);
}
