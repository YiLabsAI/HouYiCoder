//! Tests for the model-facing task tools.

use super::*;
use houyicoder_api::spawn::{
    ChildOutputView, ChildTaskSummary, SpawnFailure, SpawnHandle, SpawnOutcome,
};
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use std::sync::Arc;
use std::sync::Mutex;

/// A spawn handle serving canned fleet answers and recording what the tools
/// asked it to do, so the tests assert the tool's dispatch, not the runtime.
struct FakeFleet {
    rows: Vec<ChildTaskSummary>,
    output: ChildOutputView,
    inbox: Mutex<Vec<(String, String)>>,
    killed: Mutex<Vec<String>>,
    spawn_calls: Mutex<Vec<SpawnArgs>>,
}

impl FakeFleet {
    fn new() -> Self {
        Self {
            rows: vec![ChildTaskSummary {
                child_session_id: "child-sid".into(),
                subagent_type: "explore".into(),
                description: "find auth".into(),
                status: "running".into(),
                turn_count: 2,
                elapsed_ms: 1_000,
            }],
            output: ChildOutputView {
                child_session_id: "child-sid".into(),
                status: "completed".into(),
                summary: Some("the child answer".into()),
                text: "answer".into(),
                total_chars: 6,
                offset: 0,
            },
            inbox: Mutex::new(Vec::new()),
            killed: Mutex::new(Vec::new()),
            spawn_calls: Mutex::new(Vec::new()),
        }
    }
}

impl SpawnHandle for FakeFleet {
    fn spawn(
        &self,
        _ctx: &ToolCtx,
        args: SpawnArgs,
    ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
        self.spawn_calls.lock().unwrap().push(args);
        Box::pin(async { Ok(SpawnOutcome::background_started("child-sid")) })
    }
    fn send_to_child_inbox(&self, child_id: &str, text: String) -> Result<(), String> {
        self.inbox
            .lock()
            .unwrap()
            .push((child_id.to_string(), text));
        Ok(())
    }
    fn kill_child(&self, child_id: &str) -> bool {
        self.killed.lock().unwrap().push(child_id.to_string());
        true
    }
    fn list_children(&self, _parent: SessionId) -> PFut<'_, Vec<ChildTaskSummary>> {
        let rows = self.rows.clone();
        Box::pin(async move { rows })
    }
    fn child_output(
        &self,
        _parent: SessionId,
        _child_id: &str,
        _offset: usize,
        _limit: usize,
    ) -> PFut<'_, Result<ChildOutputView, String>> {
        let view = self.output.clone();
        Box::pin(async move { Ok(view) })
    }
}

fn fleet_ctx() -> (ToolCtx, Arc<FakeFleet>) {
    let fleet = Arc::new(FakeFleet::new());
    let ctx = ToolCtx::new("c1")
        .with_session(SessionId::new())
        .with_spawn_handle(fleet.clone());
    (ctx, fleet)
}

/// The list tool forwards the fleet rows as task entries the model reads.
#[test]
fn test_task_list_reports_fleet() {
    let (ctx, _) = fleet_ctx();
    let out = pollster::block_on(TaskListTool.execute(ctx, json!({}))).unwrap();
    let task = &out["tasks"][0];
    assert_eq!(task["task_id"], "child-sid");
    assert_eq!(task["status"], "running");
    assert_eq!(task["turns"], 2);
}

/// The output tool forwards the view's window and flags whether more text
/// remains.
#[test]
fn test_task_output_reports_window() {
    let (ctx, _) = fleet_ctx();
    let out = pollster::block_on(TaskOutputTool.execute(
        ctx,
        json!({"task_id": "child-sid", "offset": 0, "limit": 4}),
    ))
    .unwrap();
    assert_eq!(out["status"], "completed");
    assert_eq!(out["output"], "answer");
    assert_eq!(out["total_chars"], 6);
    // The fake's window holds the whole text, so no more remains.
    assert_eq!(out["more"], false);
}

/// A message to a running child routes into its inbox, not a new spawn.
#[test]
fn test_send_message_routes_inbox() {
    let (ctx, fleet) = fleet_ctx();
    let out = pollster::block_on(SendMessageTool.execute(
        ctx,
        json!({"to": "child-sid", "message": "narrow the search"}),
    ))
    .unwrap();
    assert_eq!(out["delivered"], "inbox");
    assert_eq!(
        fleet.inbox.lock().unwrap()[0],
        ("child-sid".to_string(), "narrow the search".to_string())
    );
    assert!(
        fleet.spawn_calls.lock().unwrap().is_empty(),
        "a running child needs no spawn"
    );
}

/// A message to a finished child resumes it: the spawn carries the child's
/// id and runs in the background. The resume reads the child's recorded type
/// from the fleet list, so the continued boundary keeps the same agent.
#[test]
fn test_send_message_resumes_finished() {
    let (_ctx, fleet) = fleet_ctx();
    // The inbox refuses: the child is finished.
    let stalled = Arc::new(StallFleet::wrap(&fleet));
    let ctx = ToolCtx::new("c1")
        .with_session(SessionId::new())
        .with_spawn_handle(stalled);
    let out = pollster::block_on(
        SendMessageTool.execute(ctx, json!({"to": "child-sid", "message": "continue"})),
    )
    .unwrap();
    assert_eq!(out["delivered"], "resumed");
    let calls = fleet.spawn_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].resume.as_deref(), Some("child-sid"));
    assert!(calls[0].run_in_background);
    assert_eq!(calls[0].subagent_type, "explore");
}

/// Stopping a child kills it and reports the stop.
#[test]
fn test_task_stop_kills() {
    let (ctx, fleet) = fleet_ctx();
    let out =
        pollster::block_on(TaskStopTool.execute(ctx, json!({"task_id": "child-sid"}))).unwrap();
    assert_eq!(out["stopped"], true);
    assert_eq!(fleet.killed.lock().unwrap()[0], "child-sid");
}

/// An inbox refusal for a child the fleet does not hold is an error, not a
/// spawn: the model reads which id was wrong.
#[test]
fn test_send_message_rejects_unknown() {
    let (_ctx, fleet) = fleet_ctx();
    let stalled = Arc::new(StallFleet::wrap(&fleet));
    let ctx = ToolCtx::new("c1")
        .with_session(SessionId::new())
        .with_spawn_handle(stalled);
    let err = pollster::block_on(SendMessageTool.execute(
        ctx,
        json!({"to": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "message": "hi"}),
    ))
    .unwrap_err();
    assert!(err.to_string().contains("not a child"), "{err}");
}

/// An inbox that always refuses, standing in for a finished child, while the
/// recorded spawn calls still land on the shared fleet.
struct StallFleet {
    inner: Arc<FakeFleet>,
}

impl StallFleet {
    fn wrap(inner: &Arc<FakeFleet>) -> Self {
        Self {
            inner: Arc::clone(inner),
        }
    }
}

impl SpawnHandle for StallFleet {
    fn spawn(
        &self,
        ctx: &ToolCtx,
        args: SpawnArgs,
    ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
        self.inner.spawn(ctx, args)
    }
    fn send_to_child_inbox(&self, _child_id: &str, _text: String) -> Result<(), String> {
        Err("no inbox".into())
    }
    fn list_children(&self, parent: SessionId) -> PFut<'_, Vec<ChildTaskSummary>> {
        self.inner.list_children(parent)
    }
    fn child_output(
        &self,
        parent: SessionId,
        child_id: &str,
        offset: usize,
        limit: usize,
    ) -> PFut<'_, Result<ChildOutputView, String>> {
        self.inner.child_output(parent, child_id, offset, limit)
    }
}

/// Every tool refuses cleanly when no spawn handle is wired: the model reads
/// a named error, not a panic or a fabricated answer.
#[test]
fn test_tools_refuse_without_runtime() {
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    for err in [
        pollster::block_on(TaskListTool.execute(ctx.clone(), json!({}))).unwrap_err(),
        pollster::block_on(TaskOutputTool.execute(ctx.clone(), json!({"task_id": "c"})))
            .unwrap_err(),
        pollster::block_on(
            SendMessageTool.execute(ctx.clone(), json!({"to": "c", "message": "m"})),
        )
        .unwrap_err(),
        pollster::block_on(TaskStopTool.execute(ctx, json!({"task_id": "c"}))).unwrap_err(),
    ] {
        assert!(err.to_string().contains("no agent runtime"), "{err}");
    }
}

/// Every tool refuses when the dispatch carries no session, so a
/// non-interactive run cannot read another session's fleet.
#[test]
fn test_tools_refuse_without_session() {
    let fleet = Arc::new(FakeFleet::new());
    let ctx = ToolCtx::new("c1").with_spawn_handle(fleet);
    let err = pollster::block_on(TaskListTool.execute(ctx.clone(), json!({}))).unwrap_err();
    assert!(err.to_string().contains("not bound to a session"), "{err}");
    let err = pollster::block_on(TaskOutputTool.execute(ctx.clone(), json!({"task_id": "c"})))
        .unwrap_err();
    assert!(err.to_string().contains("not bound to a session"), "{err}");
    let err = pollster::block_on(
        SendMessageTool.execute(ctx.clone(), json!({"to": "c", "message": "m"})),
    )
    .unwrap_err();
    assert!(err.to_string().contains("not bound to a session"), "{err}");
}

/// A missing required field is a validation error naming the field, so the
/// model can correct the call instead of reading a generic failure.
#[test]
fn test_missing_fields_rejected() {
    let (ctx, _) = fleet_ctx();
    let err = pollster::block_on(TaskOutputTool.execute(ctx.clone(), json!({}))).unwrap_err();
    assert!(err.to_string().contains("task_id"), "{err}");
    let err =
        pollster::block_on(SendMessageTool.execute(ctx, json!({"to": "child-sid"}))).unwrap_err();
    assert!(err.to_string().contains("message"), "{err}");
}

/// A resume the runtime refuses surfaces the spawn failure to the model
/// rather than reporting the message delivered.
#[test]
fn test_send_message_surfaces_refusal() {
    struct RefusingFleet(FakeFleet);
    impl SpawnHandle for RefusingFleet {
        fn spawn(
            &self,
            _ctx: &ToolCtx,
            _args: SpawnArgs,
        ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
            Box::pin(async { Err(SpawnFailure::Recursive) })
        }
        fn send_to_child_inbox(&self, _child_id: &str, _text: String) -> Result<(), String> {
            Err("finished".into())
        }
        fn list_children(&self, parent: SessionId) -> PFut<'_, Vec<ChildTaskSummary>> {
            self.0.list_children(parent)
        }
        fn child_output(
            &self,
            parent: SessionId,
            child_id: &str,
            offset: usize,
            limit: usize,
        ) -> PFut<'_, Result<ChildOutputView, String>> {
            self.0.child_output(parent, child_id, offset, limit)
        }
    }
    let ctx = ToolCtx::new("c1")
        .with_session(SessionId::new())
        .with_spawn_handle(Arc::new(RefusingFleet(FakeFleet::new())));
    let err = pollster::block_on(
        SendMessageTool.execute(ctx, json!({"to": "child-sid", "message": "continue"})),
    )
    .unwrap_err();
    assert!(err.to_string().contains("Recursive"), "{err}");
}

/// Each tool's surface: its name, a non-empty description, a schema whose
/// required fields match the tool's contract, and the permission axes it
/// declares. The read tools claim read-only so the gate never asks for a
/// fleet glance; the steering and stop tools claim neither read-only nor
/// destructive, so they route through the mode gate rather than a blanket
/// allow.
#[test]
fn test_tool_surfaces() {
    let tools: Vec<(&str, &dyn Tool)> = vec![
        ("task_list", &TaskListTool),
        ("task_output", &TaskOutputTool),
        ("send_message", &SendMessageTool),
        ("task_stop", &TaskStopTool),
    ];
    for (name, tool) in tools {
        assert_eq!(tool.name(), name);
        assert!(!tool.description().is_empty(), "{name} describes itself");
        assert!(
            tool.input_schema().get("type").is_some(),
            "{name} declares a schema"
        );
    }
    assert!(TaskListTool.is_read_only());
    assert!(TaskOutputTool.is_read_only());
    assert!(!SendMessageTool.is_read_only());
    assert!(!TaskStopTool.is_read_only());
    for tool in [
        &TaskListTool as &dyn Tool,
        &TaskOutputTool,
        &SendMessageTool,
        &TaskStopTool,
    ] {
        assert!(!tool.is_destructive(), "{} is not destructive", tool.name());
    }
}

/// The schemas carry their required fields: a caller reading the schema
/// alone knows which arguments are mandatory.
#[test]
fn test_required_fields_declared() {
    let output = TaskOutputTool.input_schema();
    let required = output["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "task_id"));
    let send = SendMessageTool.input_schema();
    let required = send["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "to"));
    assert!(required.iter().any(|v| v == "message"));
    let stop = TaskStopTool.input_schema();
    let required = stop["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "task_id"));
}

/// An empty task list answers with a note, not a bare empty array, so the
/// model reads "nothing delegated" rather than guessing whether the call
/// failed.
#[test]
fn test_task_list_empty_notes() {
    struct EmptyFleet;
    impl SpawnHandle for EmptyFleet {
        fn spawn(
            &self,
            _ctx: &ToolCtx,
            _args: SpawnArgs,
        ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
            Box::pin(async { Err(SpawnFailure::Recursive) })
        }
        fn list_children(&self, _parent: SessionId) -> PFut<'_, Vec<ChildTaskSummary>> {
            Box::pin(async { Vec::new() })
        }
    }
    let ctx = ToolCtx::new("c1")
        .with_session(SessionId::new())
        .with_spawn_handle(Arc::new(EmptyFleet));
    let out = pollster::block_on(TaskListTool.execute(ctx, json!({}))).unwrap();
    assert_eq!(out["tasks"].as_array().unwrap().len(), 0);
    assert!(out["note"].is_string(), "the empty state says so: {out}");
}

/// A stop aimed at a child this parent never delegated is refused before
/// the kill is called: one session cannot end another session's child even
/// though the runtime behind the handle might know the id.
#[test]
fn test_stop_rejects_foreign_child() {
    let (ctx, fleet) = fleet_ctx();
    let err = pollster::block_on(
        TaskStopTool.execute(ctx, json!({"task_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV"})),
    )
    .unwrap_err();
    assert!(err.to_string().contains("not a child"), "{err}");
    assert!(
        fleet.killed.lock().unwrap().is_empty(),
        "the refusal happens before any kill"
    );
}

/// A message aimed at a foreign id never reaches an inbox, even a live one:
/// the ownership check precedes the delivery, so a cross-session id cannot
/// steer another session's child.
#[test]
fn test_send_refuses_before_inbox() {
    let (ctx, fleet) = fleet_ctx();
    let foreign = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let err = pollster::block_on(
        SendMessageTool.execute(ctx, json!({"to": foreign, "message": "steer"})),
    )
    .unwrap_err();
    assert!(err.to_string().contains("not a child"), "{err}");
    assert!(
        fleet.inbox.lock().unwrap().is_empty(),
        "no inbox delivery for a foreign id"
    );
}

/// The stop tool asks for approval: ending a child the session paid for is a
/// gated action, so an ask mode holds it for the user.
#[test]
fn test_stop_requires_approval() {
    assert!(TaskStopTool.requires_approval());
    assert!(!TaskListTool.requires_approval());
    assert!(!TaskOutputTool.requires_approval());
    assert!(!SendMessageTool.requires_approval());
}
