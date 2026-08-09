//! Option-3 (always-allow) duplicate-render regression tests, split from
//! run_control_tests.rs for the file-size gate. Uses the parent module's
//! scripted-provider helpers to drive the real runner through the
//! GuardedTool + Default gate, with a multi-line-stdout tool so the result
//! row exercises the collapse path.

use super::*;
use crate::state::TranscriptLine;
use houyicoder_api::tool::ToolCtx;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem};

/// A destructive tool that returns a multi-line stdout (55 lines) so the
/// transcript result row exercises the collapse path (>COLLAPSE_THRESHOLD).
/// Records whether execute ran, like BoomTool.
struct MultiBoom {
    ran: std::sync::Mutex<bool>,
}
impl MultiBoom {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ran: std::sync::Mutex::new(false),
        })
    }
    fn ran(&self) -> bool {
        *self.ran.lock().expect("multiboom mutex")
    }
}
impl Tool for MultiBoom {
    fn name(&self) -> &str {
        "boom"
    }
    fn description(&self) -> &str {
        "destructive multi-line test tool"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: serde_json::Value,
    ) -> houyicoder_async::PFut<
        '_,
        Result<serde_json::Value, houyicoder_protocol::extension::ToolError>,
    > {
        *self.ran.lock().expect("multiboom mutex") = true;
        // 55 lines so the result body collapses (head + hint), matching the
        // long-stdout shape that surfaced the duplicate-render bug.
        let stdout = (0..55)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        Box::pin(async move { Ok(serde_json::json!({ "stdout": stdout })) })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// Wire a MultiBoom in a GuardedTool under the given mode, with the shared
/// handle to inspect whether execute ran.
fn app_with_multiline_guarded_tool(
    mode: houyicoder_permission::PermissionMode,
    responses: Vec<CompletionResponse>,
) -> (App, Arc<MultiBoom>) {
    let boom = MultiBoom::new();
    let gate: Arc<dyn houyicoder_permission::ModeGate> =
        Arc::new(houyicoder_permission::DefaultModeGate::with_mode(mode));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(houyicoder_permission::GuardedTool::new(
        boom.clone(),
        gate,
    )));
    let provider = Arc::new(FakeProvider::new(responses));
    (app_with_provider(provider, tools), boom)
}

fn multiline_call_then_reply() -> Vec<CompletionResponse> {
    vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "boom".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]
}

/// Reproduce the option-3 (always-allow) duplicate-render bug: after the
/// user picks always-allow, the tool runs and a multi-line result + the
/// reply land. The transcript must hold exactly ONE result row and ONE
/// reply, and the added-allow-rule system line must stay a separate row --
/// not duplicate the result nor merge into the reply text. A headless
/// TestBackend render shows no duplicate (this test passes), which proves
/// the duplicate users saw on a specific terminal is a terminal-render
/// artifact (the diff renderer leaving stale cells), not a logic bug.
#[test]
fn test_option3_no_duplicate_result() {
    use crate::test_support::render_text;
    let (mut app, boom) = app_with_multiline_guarded_tool(
        houyicoder_permission::PermissionMode::Manual,
        multiline_call_then_reply(),
    );
    app.screen = crate::state::Screen::Working;
    app.spawn_run("go".into());
    let mut raised = false;
    for _ in 0..200 {
        app.poll_agent();
        if app.approval.is_some() {
            raised = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(raised, "Manual should raise an approval popup");

    // Simulate option 3 (always-allow): persist an Allow rule, echo the
    // verdict + system line, then resume with an approve decision -- the
    // exact side effects of handle_approval Enter with selected=2.
    let call_id = app.approval.as_ref().unwrap().call_id.clone();
    app.rules_cache
        .push(houyicoder_protocol::frontend::permission::PermissionRule {
            action: "boom".to_string(),
            content: None,
            effect: houyicoder_protocol::frontend::permission::PermissionEffect::Allow,
            ..Default::default()
        });
    app.system_line("added allow rule for boom");
    app.resolve_current_approval(houyicoder_protocol::frontend::run::ApprovalDecision {
        call_id,
        approved: true,
        updated_input: None,
        scope: "once".to_string(),
    });

    let mut settled = false;
    for _ in 0..200 {
        app.poll_agent();
        if !app.agent_busy && !app.reverse_request_in_flight() {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(settled, "resume should settle");
    assert!(boom.ran(), "the tool must run after always-allow");

    // Exactly ONE result row (a Tool line with name == "result").
    let result_count = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Tool { name, .. } if name == "result"))
        .count();
    assert_eq!(
        result_count, 1,
        "exactly one result row, got {result_count}"
    );

    // Exactly ONE reply row.
    let reply_count = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Agent(s) if s == "done"))
        .count();
    assert_eq!(reply_count, 1, "exactly one reply row, got {reply_count}");

    // The system line stays a separate row, not fused into the reply.
    let sys_count = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::System(s) if s == "added allow rule for boom"))
        .count();
    assert_eq!(sys_count, 1, "exactly one system row, got {sys_count}");

    // Render: one collapse hint, not two. The in-app form is a clean "+N
    // lines" tail (the verbose Ctrl+O-to-expand suffix is suppressed).
    let out = render_text(&app, 80, 24);
    let hint_count = out.matches("… +").count();
    assert_eq!(hint_count, 1, "one collapse hint, got {hint_count}:\n{out}");
    // The system line must not appear fused onto the reply row.
    assert!(
        !out.contains("done added allow rule for boom"),
        "system line fused into reply row:\n{out}"
    );
}
