use super::*;
use crate::todo_view::{TodoStatus, TodoView};

fn item(content: &str, status: TodoStatus) -> TodoView {
    let active_form = if status == TodoStatus::InProgress {
        Some(format!("doing {content}"))
    } else {
        None
    };
    TodoView {
        content: content.into(),
        status,
        active_form,
    }
}

fn empty_completion_map() -> HashMap<String, Instant> {
    HashMap::new()
}

fn fresh_completion_map(content: &str) -> HashMap<String, Instant> {
    let mut m = HashMap::new();
    m.insert(content.to_string(), Instant::now());
    m
}

#[test]
fn test_collapsed_picks_done_pending() {
    let todos = vec![
        item("later", TodoStatus::Pending),
        item("did", TodoStatus::Completed),
        item("now", TodoStatus::InProgress),
    ];
    let comp = fresh_completion_map("did");
    let vis = visible_collapsed(&todos, &comp, COLLAPSED_MAX);
    assert_eq!(vis.len(), 3);
    assert_eq!(vis[0].content, "now"); // active first
    assert_eq!(vis[1].content, "did"); // recent completed
    assert_eq!(vis[2].content, "later"); // next pending
}

#[test]
fn test_collapsed_no_active_order() {
    let todos = vec![
        item("did", TodoStatus::Completed),
        item("next", TodoStatus::Pending),
    ];
    let comp = fresh_completion_map("did");
    let vis = visible_collapsed(&todos, &comp, COLLAPSED_MAX);
    assert_eq!(vis.len(), 2);
    assert_eq!(vis[0].content, "did");
    assert_eq!(vis[1].content, "next");
}

#[test]
fn test_collapsed_one_item_only() {
    let todos = vec![item("solo", TodoStatus::InProgress)];
    let vis = visible_collapsed(&todos, &empty_completion_map(), COLLAPSED_MAX);
    assert_eq!(vis.len(), 1);
}

#[test]
fn test_recent_done_prioritized() {
    let todos = vec![
        item("old", TodoStatus::Completed),
        item("fresh", TodoStatus::Completed),
        item("now", TodoStatus::InProgress),
    ];
    // Fresh completion is prioritized, while older completed work remains.
    let comp = fresh_completion_map("fresh");
    let vis = visible_collapsed(&todos, &comp, COLLAPSED_MAX);
    assert_eq!(vis.len(), 3);
    assert_eq!(vis[0].content, "now");
    assert_eq!(vis[1].content, "fresh");
    assert_eq!(vis[2].content, "old");
}

#[test]
fn test_old_done_retained() {
    let todos = vec![
        item("old", TodoStatus::Completed),
        item("now", TodoStatus::InProgress),
        item("next", TodoStatus::Pending),
    ];
    let vis = visible_collapsed(&todos, &empty_completion_map(), COLLAPSED_MAX);
    assert_eq!(vis.len(), 3);
    assert_eq!(vis[0].content, "now");
    assert_eq!(vis[1].content, "next");
    assert_eq!(vis[2].content, "old");
}

#[test]
fn test_collapsed_max_one_screen() {
    let todos = vec![
        item("now", TodoStatus::InProgress),
        item("did", TodoStatus::Completed),
        item("next", TodoStatus::Pending),
    ];
    let comp = fresh_completion_map("did");
    let vis = visible_collapsed(&todos, &comp, 1);
    assert_eq!(vis.len(), 1);
    assert_eq!(vis[0].content, "now"); // only active on small screen
}

#[test]
fn test_collapsed_max_zero_screen() {
    let todos = vec![item("now", TodoStatus::InProgress)];
    let vis = visible_collapsed(&todos, &empty_completion_map(), 0);
    assert!(vis.is_empty());
}

#[test]
fn test_collapsed_max_degrades_height() {
    assert_eq!(collapsed_max(7), 0);
    assert_eq!(collapsed_max(8), 1);
    assert_eq!(collapsed_max(11), 1);
    assert_eq!(collapsed_max(12), COLLAPSED_MAX);
    assert_eq!(collapsed_max(24), COLLAPSED_MAX);
}

#[test]
fn test_active_item_uses_form() {
    let line = item_line(&item("run tests", TodoStatus::InProgress));
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(joined.contains("doing run tests"));
    assert!(joined.contains("◼"));
}

#[test]
fn test_completed_item_strikes_through() {
    let line = item_line(&item("done", TodoStatus::Completed));
    assert!(
        line.spans
            .iter()
            .any(|s| { s.style.add_modifier.contains(Modifier::CROSSED_OUT) })
    );
}

#[test]
fn test_item_plain_matches_label() {
    assert_eq!(
        item_plain(&item("run tests", TodoStatus::InProgress)),
        "◼ doing run tests"
    );
    assert_eq!(item_plain(&item("done", TodoStatus::Completed)), "✔ done");
    assert_eq!(item_plain(&item("wait", TodoStatus::Pending)), "◻ wait");
}

#[test]
fn test_hidden_counts_subtract_visible() {
    let todos = vec![
        item("a", TodoStatus::Pending),
        item("b", TodoStatus::Pending),
        item("c", TodoStatus::Pending),
        item("d", TodoStatus::Pending),
        item("e", TodoStatus::InProgress),
        item("f", TodoStatus::Completed),
    ];
    let comp = fresh_completion_map("f");
    let vis = visible_collapsed(&todos, &comp, COLLAPSED_MAX);
    assert_eq!(vis.len(), 3); // active + recent completed + first pending
    let hidden = todos.len() - vis.len();
    assert_eq!(hidden, 3);
    let vis_pending = vis
        .iter()
        .filter(|t| t.status == TodoStatus::Pending)
        .count();
    let hidden_pending = count_by_status(&todos, TodoStatus::Pending) - vis_pending;
    assert_eq!(hidden_pending, 3);
}

#[test]
fn test_glyphs_match_done_pending() {
    assert_eq!(glyph_for(TodoStatus::InProgress), ("◼", Color::Cyan, false));
    assert_eq!(glyph_for(TodoStatus::Completed), ("✔", Color::Green, true));
    assert_eq!(
        glyph_for(TodoStatus::Pending),
        ("◻", Color::DarkGray, false)
    );
}
