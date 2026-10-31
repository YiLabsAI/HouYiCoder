//! Tests for this module.

use std::sync::Arc;
use std::sync::Mutex;

use super::*;
use houyicoder_context::ContextBackend;
use houyicoder_context::EventId;
use houyicoder_context::SessionId;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::block_ref::BLOCK_BYTES_KEY;
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;
use houyicoder_protocol::llm::InputItem;
use houyicoder_protocol::tool::is_user_shell_call_id;
use houyicoder_protocol::tool::user_shell_call_id;

use crate::agent::context::Tokenizer;
use crate::agent::context::retention::AssemblyScope;
use crate::agent::context::retention::BudgetedRetentionPolicy;
use crate::agent::context::retention::RetentionContext;
use crate::agent::context::retention::RetentionDecision;
use crate::agent::context::retention::RetentionPolicy;
use crate::agent::context::retention::TURN_RESULT_BUDGET_CAP_TOKENS;
use crate::agent::context::turn_group::assemble_model_input_with;

fn evt(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event,
    }
}

/// A 26-character event id, the length a user shell call id requires. The
/// ordinal is padded so every number keeps the same width.
fn shell_event_id(n: u8) -> String {
    format!("01ARZ3NDEKTSV4RRFFQ69G5F{n:02}")
}

fn shell_call(n: u8, command: &str) -> SessionLogEntry {
    evt(SessionEvent::ToolCall {
        call_id: user_shell_call_id(&shell_event_id(n)),
        tool: "bash".into(),
        input: serde_json::json!({ "command": command }),
    })
}

fn shell_result(n: u8, output: Value) -> SessionLogEntry {
    evt(SessionEvent::tool_result(
        user_shell_call_id(&shell_event_id(n)),
        output,
    ))
}

fn small_output() -> Value {
    serde_json::json!({ "stdout": "ok" })
}

/// A result whose serialized size dominates the command cost, so a budget can
/// be chosen that holds exactly one command.
fn large_output() -> Value {
    serde_json::json!({ "stdout": "x".repeat(400) })
}

fn bytes(text: &str) -> usize {
    text.len()
}

#[test]
fn test_keeps_newest_within_budget() {
    let events = vec![
        shell_call(1, "echo a"),
        shell_result(1, large_output()),
        shell_call(2, "echo b"),
        shell_result(2, large_output()),
        shell_call(3, "echo c"),
        shell_result(3, large_output()),
    ];
    // Each command costs a little over 450 bytes, so one fits and the next
    // does not.
    let plan = plan_shell_history(&events, 1000, &bytes);
    assert!(plan.keeps(&user_shell_call_id(&shell_event_id(3))));
    assert!(!plan.keeps(&user_shell_call_id(&shell_event_id(2))));
    assert!(
        !plan.keeps(&user_shell_call_id(&shell_event_id(1))),
        "a command that does not fit folds every older one with it"
    );
    let notice = plan.notice.expect("a folded command is announced");
    assert!(
        notice.contains("2 earlier user shell commands"),
        "the notice names how many folded: {notice}"
    );
    assert!(
        notice.contains("echo a"),
        "the notice previews them: {notice}"
    );
    assert!(plan.charged_tokens > 0);
}

#[test]
fn test_notice_only_when_folded() {
    let events = vec![shell_call(1, "echo a"), shell_result(1, small_output())];
    let plan = plan_shell_history(&events, 100_000, &bytes);
    assert!(plan.keeps(&user_shell_call_id(&shell_event_id(1))));
    assert!(
        plan.notice.is_none(),
        "nothing was folded, so nothing is said"
    );
}

/// The budget an old session was assembled under may leave no room for the
/// notice. The request still says the history is not available: a request that
/// silently drops it reads to the model as a session where nothing was run.
#[test]
fn test_zero_budget_marks_unavailable() {
    let events = vec![
        shell_call(1, "echo a"),
        shell_result(1, small_output()),
        shell_call(2, "echo b"),
        shell_result(2, small_output()),
    ];
    let plan = plan_shell_history(&events, 0, &bytes);
    assert!(!plan.keeps(&user_shell_call_id(&shell_event_id(1))));
    assert!(!plan.keeps(&user_shell_call_id(&shell_event_id(2))));
    assert_eq!(plan.notice.as_deref(), Some(ABSENT_NOTICE));
}

/// A result whose bytes live in the block store is sized as the log holds it,
/// not by the stored length its marker names. The retrieval is a separate
/// decision made later against the same budget; charging the stored length
/// here would pay for it twice and spend the quota of a command the plan
/// already afforded.
#[test]
fn test_marker_sized_as_logged() {
    let marker = serde_json::json!({
        BLOCK_REF_KEY: "a".repeat(64),
        BLOCK_BYTES_KEY: 1_048_576u64,
        "preview": "start"
    });
    let events = vec![
        shell_call(1, "echo a"),
        shell_result(1, small_output()),
        shell_call(2, "echo b"),
        shell_result(2, marker),
    ];
    let plan = plan_shell_history(&events, 4096, &bytes);
    assert!(
        plan.keeps(&user_shell_call_id(&shell_event_id(2))),
        "the marker's logged text fits, so the newest command stays"
    );
    assert!(plan.notice.is_none());
    assert!(
        plan.charged_tokens < 1024,
        "a block held in the store is not charged by its stored length: {}",
        plan.charged_tokens
    );
}

/// The absent notice costs the tokens it renders, not its byte length: the
/// charge is what the request carries, and a request with no room for
/// anything else still says the history is unavailable.
#[test]
fn test_absent_notice_charge() {
    let events = vec![
        shell_call(1, "echo a"),
        shell_result(1, small_output()),
        shell_call(2, "echo b"),
        shell_result(2, small_output()),
    ];
    let plan = plan_shell_history(&events, 0, &bytes);
    assert_eq!(plan.notice.as_deref(), Some(ABSENT_NOTICE));
    assert_eq!(
        plan.charged_tokens as usize,
        bytes(ABSENT_NOTICE),
        "the charge is the rendered notice, in the budget's unit"
    );
}

/// A test inner policy that always asks to materialize, so the budget wrapper
/// is the only thing that can demote the decision.
struct AlwaysMaterialize;

impl RetentionPolicy for AlwaysMaterialize {
    fn decide(&self, _ctx: &RetentionContext<'_>) -> RetentionDecision {
        RetentionDecision::Materialize
    }
}

/// The budget a shell history test plans against.
const TEST_BUDGET: u32 = TURN_RESULT_BUDGET_CAP_TOKENS;

/// A command the planner kept is not charged a second time when the retention
/// pre-check reaches its result: the logged term the plan charged steps aside
/// as the retrieval takes its place, so the block is admitted against what the
/// request actually still holds.
#[test]
fn test_kept_command_materializes() {
    let call_id = user_shell_call_id(&shell_event_id(1));
    let hash = "a".repeat(64);
    let marker = serde_json::json!({
        BLOCK_REF_KEY: hash,
        BLOCK_BYTES_KEY: 7000u64,
        "preview": "start"
    });
    let events = vec![
        shell_call(1, "cargo test"),
        shell_result(1, serde_json::json!({ "stdout": marker })),
    ];
    let policy = BudgetedRetentionPolicy::new(Arc::new(AlwaysMaterialize), Tokenizer::new());
    policy.set_turn_budget(TEST_BUDGET);
    let plan = plan_shell_history(&events, TEST_BUDGET, &|text| policy.count_tokens(text));
    assert!(plan.keeps(&call_id), "the newest command fits the plan");
    let charge = u64::from(plan.charged_tokens);
    let logged = plan
        .charged_result_tokens(&call_id)
        .expect("the plan reports the logged term of a kept command");
    assert!(logged > 0, "the logged marker costs something");
    policy.charge_assembly_tokens(1, plan.charged_tokens);
    // A stored size admitted only because the logged term steps aside: with
    // both terms spent the pre-check would demote the block.
    let admitted = u64::from(TEST_BUDGET) - charge + logged - 1;
    let ctx = RetentionContext {
        age_in_turns: 0,
        is_superseded: false,
        block_ref: Some(hash.as_str()),
        stored_bytes: Some(admitted),
        logged_result_tokens: Some(logged),
        assembly_id: 1,
        mutating: true,
    };
    assert_eq!(
        policy.decide(&ctx),
        RetentionDecision::Materialize,
        "the retrieval replaces the logged form instead of paying for it twice"
    );
    // The release is real, not only a projection: the next result is admitted
    // against the quota the logged form gave back.
    let following = RetentionContext {
        block_ref: Some("b"),
        stored_bytes: Some(u64::from(TEST_BUDGET) - (charge - logged) - 1),
        logged_result_tokens: None,
        ..ctx
    };
    assert_eq!(
        policy.decide(&following),
        RetentionDecision::Materialize,
        "the charge the retrieval replaced no longer holds the quota"
    );
}

/// A retrieval that reaches the budget is still demoted: replacing the logged
/// term frees that term only, so the pre-check keeps bounding what one
/// assembly may read.
#[test]
fn test_kept_command_demotes() {
    let call_id = user_shell_call_id(&shell_event_id(1));
    let marker = serde_json::json!({
        BLOCK_REF_KEY: "a".repeat(64),
        BLOCK_BYTES_KEY: 7000u64,
        "preview": "start"
    });
    let events = vec![
        shell_call(1, "cargo test"),
        shell_result(1, serde_json::json!({ "stdout": marker })),
    ];
    let policy = BudgetedRetentionPolicy::new(Arc::new(AlwaysMaterialize), Tokenizer::new());
    policy.set_turn_budget(TEST_BUDGET);
    let plan = plan_shell_history(&events, TEST_BUDGET, &|text| policy.count_tokens(text));
    assert!(plan.keeps(&call_id), "the newest command fits the plan");
    let charge = u64::from(plan.charged_tokens);
    let logged = plan
        .charged_result_tokens(&call_id)
        .expect("the plan reports the logged term of a kept command");
    policy.charge_assembly_tokens(1, plan.charged_tokens);
    let ctx = RetentionContext {
        age_in_turns: 0,
        is_superseded: false,
        block_ref: None,
        stored_bytes: Some(u64::from(TEST_BUDGET) - charge + logged),
        logged_result_tokens: Some(logged),
        assembly_id: 1,
        mutating: true,
    };
    assert_eq!(
        policy.decide(&ctx),
        RetentionDecision::Summarize,
        "a retrieval that reaches the budget is demoted"
    );
}

/// The assembly hands the retention policy the term the plan charged for a
/// kept command's result, and reports none for a result the plan never
/// charged. Without the term the budget check cannot tell that a retrieval
/// replaces the logged form.
#[test]
fn test_assembly_reports_logged_term() {
    struct Recording {
        seen: Mutex<Vec<Option<u64>>>,
    }

    impl RetentionPolicy for Recording {
        fn decide(&self, ctx: &RetentionContext<'_>) -> RetentionDecision {
            self.seen
                .lock()
                .expect("the recording is not poisoned")
                .push(ctx.logged_result_tokens);
            RetentionDecision::Materialize
        }
    }

    let backend = InMemoryBackend::new();
    let store = |value: Value| -> String {
        let bytes = serde_json::to_vec(&value).expect("the block serializes");
        pollster::block_on(backend.block_put(bytes))
            .expect("the block store holds the bytes")
            .0
    };
    let shell_marker = serde_json::json!({
        BLOCK_REF_KEY: store(serde_json::json!({ "stdout": "x".repeat(4000) })),
        BLOCK_BYTES_KEY: 4000u64,
        "preview": "start"
    });
    let model_marker = serde_json::json!({
        BLOCK_REF_KEY: store(serde_json::json!({ "content": "y".repeat(4000) })),
        BLOCK_BYTES_KEY: 4000u64,
        "preview": "start"
    });
    let events = vec![
        shell_call(1, "cargo test"),
        shell_result(1, serde_json::json!({ "stdout": shell_marker })),
        evt(SessionEvent::ToolCall {
            call_id: "call-1".into(),
            tool: "read".into(),
            input: serde_json::json!({ "path": "a.rs" }),
        }),
        evt(SessionEvent::tool_result("call-1", model_marker)),
    ];
    let expected = plan_shell_history(&events, TEST_BUDGET, &|text| text.len())
        .charged_result_tokens(&user_shell_call_id(&shell_event_id(1)));
    assert!(
        expected.is_some_and(|term| term > 0),
        "the plan charges the kept command's logged result"
    );
    let recording = Recording {
        seen: Mutex::new(Vec::new()),
    };
    let items = assemble_model_input_with(
        &events,
        Some(&backend),
        &recording,
        AssemblyScope {
            id: 1,
            mutating: true,
        },
    );
    let results = items
        .iter()
        .filter(|item| matches!(item, InputItem::ToolResult { .. }))
        .count();
    assert_eq!(results, 2, "both results reach the request");
    assert_eq!(
        *recording
            .seen
            .lock()
            .expect("the recording is not poisoned"),
        vec![expected, None],
        "the kept command reports its logged term and the model result reports none"
    );
}

/// The tokens one assembled request carries for the user shell history: the
/// kept commands' call inputs and results, plus the notice standing in for
/// the folded ones. Measured on the request itself rather than on the plan,
/// so the two accounts have to agree.
fn carried_shell_tokens(items: &[InputItem], notice: &str, count: &dyn Fn(&str) -> usize) -> usize {
    let mut total = 0usize;
    for item in items {
        match item {
            InputItem::ToolResult { call_id, output } if is_user_shell_call_id(call_id) => {
                if let Ok(text) = serde_json::to_string(output) {
                    total += count(&text);
                }
            }
            InputItem::Assistant { tool_calls, .. } => {
                for call in tool_calls.iter().filter(|c| is_user_shell_call_id(&c.id)) {
                    if let Ok(text) = serde_json::to_string(&call.input) {
                        total += count(&text);
                    }
                }
            }
            InputItem::User { content } if content.contains(notice) => {
                total += count(content);
            }
            _ => {}
        }
    }
    total
}

/// Fifty commands in one session still assemble into a request whose shell
/// history fits the turn budget: the newest commands stay, the rest fold into
/// one notice, and the text the request carries measures under the same
/// budget the plan was made against.
#[test]
fn test_fifty_commands_stay_bounded() {
    // Each output alone costs a fraction of the budget, so fifty of them only
    // fit when the request folds.
    let bulk = serde_json::json!({ "stdout": "x".repeat(4000) });
    let events: Vec<SessionLogEntry> = (1..=50u8)
        .flat_map(|n| {
            [
                shell_call(n, "cargo test --all-targets"),
                shell_result(n, bulk.clone()),
            ]
        })
        .collect();
    let policy = BudgetedRetentionPolicy::new(Arc::new(AlwaysMaterialize), Tokenizer::new());
    policy.set_turn_budget(TEST_BUDGET);
    let plan = plan_shell_history(&events, TEST_BUDGET, &|text| policy.count_tokens(text));
    assert!(
        plan.kept_count() < 50,
        "fifty full outputs cannot all fit: kept {}",
        plan.kept_count()
    );
    let notice = plan
        .notice
        .as_deref()
        .expect("the folded commands are announced");
    assert!(
        plan.charged_tokens <= TEST_BUDGET,
        "the plan charges {} against a budget of {TEST_BUDGET}",
        plan.charged_tokens
    );
    let items = assemble_model_input_with(
        &events,
        None,
        &policy,
        AssemblyScope {
            id: 1,
            mutating: true,
        },
    );
    let carried = carried_shell_tokens(&items, notice, &|text| policy.count_tokens(text));
    assert!(
        carried <= TEST_BUDGET as usize,
        "the request carries {carried} tokens of shell history, budget {TEST_BUDGET}"
    );
}

/// A command is one line in the notice: a multi-line command keeps its first
/// line only, so the notice stays one row whatever the command held.
#[test]
fn test_preview_is_single_line() {
    let long = format!("echo {}\nsecond line\nthird", "x".repeat(80));
    assert!(!command_preview(&long).contains('\n'));
    let preview = command_preview(&long);
    assert_eq!(preview.chars().count(), NOTICE_PREVIEW_CHARS + 1);
    assert!(preview.ends_with('…'));
}

/// The notice lists at most a fixed number of previews however many commands
/// were folded, so its size does not grow with the history it stands for.
#[test]
fn test_notice_previews_are_bounded() {
    let events: Vec<SessionLogEntry> = (1..=20)
        .flat_map(|n| [shell_call(n, "true"), shell_result(n, small_output())])
        .collect();
    // One command is wider than the room beside the notice hold, so all
    // twenty fold.
    let plan = plan_shell_history(&events, NOTICE_RESERVE_TOKENS + 66, &bytes);
    let notice = plan.notice.expect("folded commands are announced");
    assert!(
        notice.contains("20 earlier user shell commands"),
        "{notice}"
    );
    assert_eq!(notice.matches("true").count(), NOTICE_PREVIEWS);
}

/// A command whose result never reached the log is still a command the
/// request would carry, so it is planned like any other.
#[test]
fn test_call_without_result_counted() {
    let events = vec![shell_call(1, "echo a")];
    let plan = plan_shell_history(&events, 100_000, &bytes);
    assert!(plan.keeps(&user_shell_call_id(&shell_event_id(1))));
}

/// The request carries the call input serialized, where every quote and
/// backslash is escaped. A command built from 200 quotes is 200 bytes of text
/// but 415 bytes once serialized; a budget between the two keeps it only if
/// the planner sizes the bare text.
#[test]
fn test_escaped_command_charges_serialized() {
    let command = "\"".repeat(200);
    let events = vec![shell_call(1, &command)];
    let budget = NOTICE_RESERVE_TOKENS + 300;
    let plan = plan_shell_history(&events, budget, &bytes);
    assert!(
        !plan.keeps(&user_shell_call_id(&shell_event_id(1))),
        "the escaped input exceeds the command budget, so the command folds"
    );
    assert!(plan.notice.is_some());
}
