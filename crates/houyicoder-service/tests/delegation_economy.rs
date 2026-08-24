//! Delegation economy gate.
//!
//! A pre-commit proxy for the token cost of delegation vs inline. This binary
//! has no fleet telemetry, so a deterministic fixture stands in for the
//! measurement channel a deployed fleet gets from run-volume sampling. The
//! gate's reach is harness overhead only: it checks that delegating keeps the
//! parent's assembled prompt small (the child runs in a fresh context).
//! Strategy quality (real delegation behavior, re-exploration, prompt quality)
//! is measured by the eval + dogfood runs, NOT here — a green gate does not
//! prove delegation is a good idea, only that the harness does not inflate
//! the parent's context.
//!
//! Token counts come from the ACTUAL prompts the engine assembles, measured
//! by a real BPE tokenizer (Tokenizer::real, opt out of the fast chars-per-4
//! path). The scripted provider's Usage field is NOT trusted (it would be
//! tautological — the gate would assert on numbers the fixture author made
//! up). The scripted responses only drive the scenario deterministically.
//!
//! #[ignore] + not in the nextest blocking-exclusion list, so it runs in
//! make verify's blocking ignored-suite (the binary name delegation_economy
//! matches no exclusion). NOT report-only: a regression here fails verify.

#![cfg(test)]

use std::sync::{Arc, Mutex};

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::SessionId;
use houyicoder_core::agent::multi_agent::registry::AgentRegistry;
use houyicoder_core::agent::multi_agent::registry::{BuiltInRegistry, built_in_all};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{AgentTool, Runner, Tokenizer, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_provider::FakeProvider;
use houyicoder_service::composition::multi_agent::{MultiAgentDeps, MultiAgentRuntime};
use houyicoder_session::SessionStore;

/// A reference text the real BPE tokenizer counts at a known value. Pinned so
/// that swapping Tokenizer::real for Tokenizer::new (the HOUYICODER_FAST_TOKENS
/// fast path, chars/4) immediately reds the self-check — only the real BPE
/// produces this count. The value is computed once via Tokenizer::real and
/// pinned; re-baseline deliberately if the BPE table changes.
const GOLDEN_SAMPLE: &str = "审计认证模块：auth.rs 内 fn verify(token: &str) -> bool \
    存在时序泄漏（constant-time 比较缺失）。修复需用 \
    subtle::ConstantTimeEq；并补 3 条回归测试。";

/// The long analysis the inline parent produces (and the delegation parent
/// must NOT carry). Content is a plausible security audit; only the token
/// count matters (it is the displaced content), so it stays concise + clean
/// rather than padded. The displacement bound is volume-independent (see
/// below), so the blob does not need to be large.
const ANALYSIS: &str = "## auth audit — findings\n\
    1. auth.rs:42 verify() short-circuits on the first byte mismatch — a \
    timing leak. an attacker times responses to walk the token byte by byte. \
    fix: compare with a constant-time eq + add a timing harness test.\n\
    2. session.rs:118 the session token is stored in clear in the sidecar \
    JSON. a backup or snapshot leaks every live token. fix: encrypt the \
    sidecar at rest with the OS keystore, behind a per-platform cfg.\n\
    3. login.rs:67 the password endpoint has no rate limit; a stuffing run \
    lands every attempt. fix: a token-bucket limiter keyed by ip + account, \
    backed by redis so a scale-out keeps the limit.\n\
    4. middleware.rs:90 CORS reflects any origin in production. fix: pin the \
    origin to the deploy domain, reject mismatches.\n\
    5. keys.rs:12 the RSA key is 2048 bits, under the 3072 floor. fix: \
    rotate to 3072, dual-sign during the migration, retire the old after TTL.\n\
    6. auth.rs:90 issue() mints a 30-day token with no rotation. fix: 1h TTL \
    + a refresh token + a revoke-on-logout denylist.\n\
    7. session.rs:200 logout does not invalidate the sidecar. fix: write a \
    revoked-at timestamp + short-circuit verify on it.\n\
    8. middleware.rs:130 the auth header is parsed by a hand-rolled split \
    that panics on a malformed value. fix: use a typed header + add a fuzz \
    target.\n\
    remediation order: 1 + 4 same-day (low cost, high impact). 2 + 6 in the \
    week (storage + issue path). 3 + 7 need the redis backend, land together. \
    5 is a migration for the next deploy. 8 bundles with 3. each finding gets \
    a regression test; the limiter needs a fake-clock fixture; the keystore \
    needs a per-platform cfg test; the key rotation needs a dual-sign \
    integration test.";

/// A short summary the child returns to the parent (the displaced result).
/// Must stay small relative to ANALYSIS so the displacement bound holds.
const CHILD_SUMMARY: &str = "auth audit done: 5 findings (timing leak, cleartext token, no \
    rate limit, wildcard CORS, short RSA key). see child transcript for detail.";

/// The large pre-context the parent accumulates before the sub-task (the
/// realistic case: the parent already has context when it delegates).
const PRE_CONTEXT: &str = "project: a Rust auth service. modules: auth.rs (verify, issue), \
    session.rs (sidecar), login.rs (handler), middleware.rs (cors), keys.rs (rsa). \
    stack: axum, tokio, rsa, subtle. ci: github actions, ubuntu + windows. \
    the service is deployed behind an nginx reverse proxy with a 10s timeout. \
    logs ship to loki. the on-call rotation is 3 engineers. the last incident \
    was a 401 storm from a stale token cache, fixed in a2f3c1. the auth module \
    has 412 lines and 38 tests, 4 ignored (windows path). coverage 71%.";

const SUB_TASK: &str = "audit the auth module for security issues and report.";

/// A unique, newline-free fragment of ANALYSIS. The inclusion check searches
/// for this, not the full ANALYSIS, because the agent tool's tool_result is
/// JSON and serde_json escapes newlines — the full ANALYSIS (real newlines)
/// would never substring-match the escaped tool_result. This fragment has no
/// JSON-special chars and does not appear in CHILD_SUMMARY, so it cleanly
/// separates "the child's full work leaked" from "only the summary returned".
const ANALYSIS_FRAGMENT: &str = "short-circuits on the first byte mismatch";

/// One recorded provider call: the assembled prompt text + its real-BPE token
/// count, captured at call time so the fixture measures what the engine
/// actually sent, not the scripted Usage field.
#[derive(Clone)]
struct CallRecord {
    prompt_text: String,
    prompt_tokens: u32,
}

/// A provider wrapper that records each CompletionRequest's assembled prompt
/// text + real-BPE token count, then delegates to a scripted FakeProvider.
/// Defined in the service test crate so it can use the engine crate's
/// Tokenizer (the provider leaf crate cannot depend on the engine layer).
struct RecordingProvider {
    inner: Arc<dyn ModelProvider>,
    tok: Arc<Tokenizer>,
    records: Mutex<Vec<CallRecord>>,
}

impl RecordingProvider {
    fn new(inner: Arc<dyn ModelProvider>, tok: Arc<Tokenizer>) -> Self {
        Self {
            inner,
            tok,
            records: Mutex::new(Vec::new()),
        }
    }

    fn records(&self) -> Vec<CallRecord> {
        self.records.lock().expect("records lock").clone()
    }

    /// Assemble the prompt text the engine sent (instructions + every input
    /// item's text content, including tool-call inputs on assistant turns) so
    /// the inclusion check + token count measure the full prompt. Skipping the
    /// tool_calls would undercount the delegation parent (its agent-tool-call
    /// turn) and make the displacement bound asymmetric + lenient.
    fn prompt_text(req: &CompletionRequest) -> String {
        use houyicoder_protocol::llm::InputItem;
        let mut out = String::new();
        out.push_str(&req.instructions);
        for item in &req.input {
            match item {
                InputItem::User { content } | InputItem::Assistant { content, .. } => {
                    out.push_str(content);
                }
                InputItem::ToolResult { output, .. } => {
                    out.push_str(&output.to_string());
                }
            }
            // An assistant turn may carry tool calls alongside its text; their
            // inputs are part of what the model sees, so serialize them too.
            if let InputItem::Assistant { tool_calls, .. } = item {
                for tc in tool_calls {
                    out.push_str(&tc.name);
                    out.push_str(&tc.input.to_string());
                }
            }
            out.push('\n');
        }
        out
    }

    fn record(&self, req: &CompletionRequest) {
        let text = Self::prompt_text(req);
        let tokens = self.tok.count(&text);
        self.records.lock().expect("records lock").push(CallRecord {
            prompt_text: text,
            prompt_tokens: tokens,
        });
    }
}

impl ModelProvider for RecordingProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.record(&req);
        self.inner.complete(req)
    }

    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.record(&req);
        self.inner.stream(req)
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }
}

fn text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text {
            text: text.to_string(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn agent_tool_call(id: &str, subagent_type: &str, prompt: &str, description: &str) -> OutputItem {
    OutputItem::ToolCall {
        id: id.to_string(),
        name: "agent".to_string(),
        input: serde_json::json!({
            "subagent_type": subagent_type,
            "prompt": prompt,
            "description": description,
        }),
    }
}

/// Build a parent Runner + its RecordingProvider. When spawn is true the
/// runner carries a real MultiAgentRuntime (production spawn path) so the
/// agent tool runs the child with a fresh child session + a production
/// system prompt. The child's scripted responses are separate from the
/// parent's so calls attribute cleanly.
struct Harness {
    runner: Arc<Runner>,
    parent: Arc<RecordingProvider>,
    /// The child's recording provider; None in inline mode (no spawn).
    child: Option<Arc<RecordingProvider>>,
    session: SessionId,
}

fn build_harness(
    parent_responses: Vec<CompletionResponse>,
    child_responses: Vec<CompletionResponse>,
    spawn: bool,
) -> Harness {
    let tok = Arc::new(Tokenizer::real());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(AgentTool::new(registry.clone())));
    let config = RunnerConfig::default();
    let parent = Arc::new(RecordingProvider::new(
        Arc::new(FakeProvider::new(parent_responses)),
        tok.clone(),
    ));
    let parent_dyn: Arc<dyn ModelProvider> = parent.clone();
    let mut builder =
        Runner::with_shared_store(store.clone(), parent_dyn, tools.clone(), config.clone());
    if spawn {
        let child = Arc::new(RecordingProvider::new(
            Arc::new(FakeProvider::new(child_responses)),
            tok.clone(),
        ));
        let child_dyn: Arc<dyn ModelProvider> = child.clone();
        let runtime = MultiAgentRuntime::new(MultiAgentDeps {
            registry: registry.clone(),
            store: store.clone(),
            provider: child_dyn,
            tools,
            config,
            worktree_controller: None,
            workspace: Some(std::path::PathBuf::from("/tmp")),
        });
        builder = builder.with_spawn_handle(Arc::new(runtime));
        return Harness {
            runner: Arc::new(builder),
            parent,
            child: Some(child),
            session: SessionId::new(),
        };
    }
    Harness {
        runner: Arc::new(builder),
        parent,
        child: None,
        session: SessionId::new(),
    }
}

fn run(rt: &tokio::runtime::Runtime, runner: &Runner, session: SessionId, text: &str) {
    let outcome = rt.block_on(async { runner.run(session, text.to_string()).await });
    assert!(outcome.is_ok(), "run({text}) failed: {:?}", outcome.err());
}

/// Peak prompt tokens across all recorded calls.
fn peak(records: &[CallRecord]) -> u32 {
    records.iter().map(|r| r.prompt_tokens).max().unwrap_or(0)
}

/// Total prompt tokens across all recorded calls.
fn total(records: &[CallRecord]) -> u32 {
    records.iter().map(|r| r.prompt_tokens).sum()
}

/// True when needle appears in any recorded prompt text.
fn any_contains(records: &[CallRecord], needle: &str) -> bool {
    records.iter().any(|r| r.prompt_text.contains(needle))
}

/// The token count of the system-prompt prefix on the first recorded call —
/// the text before the marker (the first user message). Both modes must share
/// it or their peaks are not comparable. The marker must be present; if it is
/// not, a runner bug broke the user message and far more than this check.
fn first_prefix(records: &[CallRecord], tok: &Tokenizer, marker: &str) -> u32 {
    records
        .first()
        .map(|r| {
            let pos = r
                .prompt_text
                .find(marker)
                .expect("first call must carry the pre-context marker");
            tok.count(&r.prompt_text[..pos])
        })
        .unwrap_or(0)
}

/// The inline parent does the whole sub-task itself: its context carries the
/// long analysis into the final turn's prompt. The delegation parent delegates:
/// the analysis stays in the child's fresh context, the parent carries only the
/// short summary. The load-bearing assertions are volume-independent; the
/// 60%/1.5x numbers are reported, not gated, because their discriminating
/// power scales with the chosen blob size rather than the mechanism.
#[ignore]
#[test]
fn test_delegation_displaces_child() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // --- self-check: the fixture uses a real BPE, not the fast chars/4 path ---
    let tok = Tokenizer::real();
    let pinned = tok.count(GOLDEN_SAMPLE);
    // Re-baseline deliberately if the BPE table changes; the point is that
    // only real() produces this count, so swapping real->new reds here.
    assert_eq!(
        pinned, 51,
        "golden BPE count drifted; re-pin after a tiktoken table change"
    );

    // --- inline mode: parent does the sub-task, analysis lands in its context ---
    let inline = build_harness(
        vec![
            text_response("ack"),
            text_response(ANALYSIS),
            text_response("summary"),
        ],
        Vec::new(),
        false,
    );
    run(&rt, &inline.runner, inline.session, PRE_CONTEXT);
    run(&rt, &inline.runner, inline.session, SUB_TASK);
    run(&rt, &inline.runner, inline.session, "summarize");
    let inline_records = inline.parent.records();
    let inline_peak = peak(&inline_records);
    assert!(
        any_contains(&inline_records, ANALYSIS_FRAGMENT),
        "inline parent must carry the analysis in its context"
    );

    // --- delegation mode: parent delegates, child runs fresh, parent carries only the summary ---
    let delegation = build_harness(
        vec![
            text_response("ack"),
            CompletionResponse {
                output: vec![agent_tool_call("ag1", "explore", SUB_TASK, "auth audit")],
                usage: Usage::default(),
                model: "test".into(),
            },
            text_response("done"),
            text_response("summary"),
        ],
        vec![text_response(CHILD_SUMMARY)],
        true,
    );
    run(&rt, &delegation.runner, delegation.session, PRE_CONTEXT);
    run(&rt, &delegation.runner, delegation.session, SUB_TASK);
    run(&rt, &delegation.runner, delegation.session, "summarize");
    let delegation_records = delegation.parent.records();
    let delegation_peak = peak(&delegation_records);

    // --- load-bearing assertion 1: inclusion ---
    // The analysis MUST NOT appear in any parent prompt in delegation mode.
    // This is the invariant delegation buys: the child's full work does not
    // leak into the parent's assembled context.
    assert!(
        !any_contains(&delegation_records, ANALYSIS_FRAGMENT),
        "delegation parent must not carry the child's full analysis; \
         the harness would be inflating the parent context"
    );

    // --- load-bearing assertion 2: displacement ---
    // The savings must cover the displaced content (the analysis) minus the
    // legitimate costs delegation always pays: the child's summary (the
    // parent receives it in place of the analysis) + a fixed harness-metadata
    // overhead (the agent-tool-call, the agentId UUID, the usage fields the
    // tool_result carries). METADATA_BUDGET covers that overhead (measured
    // ~90 tok; 150 flags any growth). This bound is volume-independent: it
    // holds at any analysis size large enough to displace, and a small
    // delegation where overhead dominates is a real economy finding, not a
    // harness bug, so the bound must not fail it.
    const METADATA_BUDGET: u32 = 150;
    let analysis_tokens = tok.count(ANALYSIS);
    let summary_tokens = tok.count(CHILD_SUMMARY);
    let saved = inline_peak.saturating_sub(delegation_peak);
    let floor = analysis_tokens.saturating_sub(summary_tokens + METADATA_BUDGET);
    assert!(
        saved >= floor,
        "displacement {} must cover analysis({}) - summary({}) - metadata_budget({}) = {}; \
         inline_peak={}, delegation_peak={}",
        saved,
        analysis_tokens,
        summary_tokens,
        METADATA_BUDGET,
        floor,
        inline_peak,
        delegation_peak
    );

    // --- load-bearing assertion 3: prefix parity ---
    // Both modes' parent system-prompt prefix must be equal, else a future
    // single-side script change silently makes the two peaks incomparable.
    let inline_prefix = first_prefix(&inline_records, &tok, PRE_CONTEXT);
    let delegation_prefix = first_prefix(&delegation_records, &tok, PRE_CONTEXT);
    assert_eq!(
        inline_prefix, delegation_prefix,
        "parent prefix must be identical across modes (else peaks are not comparable)"
    );

    // --- report values (non-gated): the harness must not cost more than 1.5x ---
    // The ratio's denominator is the inline parent total; the numerator adds the
    // child's total (the fresh-context runs). Reported, not gated: the ratio's
    // discriminating power scales with the chosen analysis size, so it is a
    // sanity number, not a load-bearing gate.
    let delegation_total = total(&delegation_records);
    let child_total = delegation
        .child
        .as_ref()
        .map(|c| total(&c.records()))
        .unwrap_or(0);
    let ratio = (delegation_total + child_total) as f64 / total(&inline_records) as f64;
    eprintln!(
        "delegation economy (report-only, not gated — reach is harness overhead, not strategy): \
         displacement={saved}tok analysis={analysis_tokens}tok summary={summary_tokens}tok \
         inline_peak={inline_peak} delegation_peak={delegation_peak} total_ratio={ratio:.2}"
    );
}

/// Mutation verification: when the child returns its full analysis as the
/// result (no displacement), the inclusion invariant MUST break — the
/// analysis appears in a parent prompt. This proves the inclusion check has
/// teeth; without it the gate would be theater.
#[ignore]
#[test]
fn test_gate_catches_undisplaced() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let delegation = build_harness(
        vec![
            text_response("ack"),
            CompletionResponse {
                output: vec![agent_tool_call("ag1", "explore", SUB_TASK, "auth audit")],
                usage: Usage::default(),
                model: "test".into(),
            },
            text_response("done"),
            text_response("summary"),
        ],
        // Mutation: child returns the FULL analysis as its final result, so
        // build_tool_result carries it verbatim into the parent's context.
        vec![text_response(ANALYSIS)],
        true,
    );
    run(&rt, &delegation.runner, delegation.session, PRE_CONTEXT);
    run(&rt, &delegation.runner, delegation.session, SUB_TASK);
    run(&rt, &delegation.runner, delegation.session, "summarize");
    let records = delegation.parent.records();
    assert!(
        any_contains(&records, ANALYSIS_FRAGMENT),
        "the gate must catch the undisplaced case: when the child's full \
         analysis leaks into the parent, the inclusion check reds"
    );
}
