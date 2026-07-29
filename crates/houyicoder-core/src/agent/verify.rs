//! verify gate — a harness-level checkpoint that runs after a run completes
//! and verifies the output (make check + adversarial verify) before the run is
//! declared done. This is the mechanism that lets the agent self-verify
//! instead of relying on the caller to catch regressions: the loop runs the
//! gate, and a failure is fed back to the model so it can fix its own work.
//!
//! The make-check gate runs the workspace make check through the
//! ProcessLauncher port so the spawn chokepoint (audit, fence policy) applies
//! uniformly. The gate is optional; a runner with no gate behaves exactly as
//! before.

use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_api::launcher::{
    LauncherExit, ProcessLauncher, SpawnPolicy, SpawnRequest, StdProcessLauncher,
};
use houyicoder_api::session::SessionLog;
use houyicoder_async::PFut;
use houyicoder_context::SessionId;

/// A post-run verification checkpoint. The runner calls verify after a run
/// reaches FinalOutput. Ok(()) means the output passes; Err means the model
/// work failed verification and the caller should re-prompt it to fix the
/// findings. The gate runs at the harness level (not as a tool call) so it
/// cannot be skipped by the model.
///
/// Object-safe via PFut so the runner holds Arc<dyn VerifyGate>, mirroring the
/// Tool and ModelProvider seams.
pub trait VerifyGate: Send + Sync {
    /// Run verification for a completed run. Ok(()) passes; Err carries the
    /// failing checks plus actionable hints the caller can feed back to the
    /// model so it knows what to fix.
    fn verify(
        &self,
        session: SessionId,
        store: &dyn SessionLog,
    ) -> PFut<'_, Result<(), VerifyFailure>>;
}

/// A verification failure. checks names the failing verification steps with a
/// short summary each; suggestions carries actionable hints the caller can
/// feed back to the model so it knows what to fix.
#[derive(Debug, Clone, Default)]
pub struct VerifyFailure {
    /// One entry per failing check, e.g. clippy with 2 errors.
    pub checks: Vec<String>,
    /// Actionable hints, e.g. run cargo clippy --fix then re-run.
    pub suggestions: Vec<String>,
}

impl VerifyFailure {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, check: impl Into<String>) {
        self.checks.push(check.into());
    }
}

/// The captured output of running make check (or whatever the gate shelled
/// out to). success is the exit status; stdout and stderr are merged output
/// used to locate the failing step.
#[derive(Debug, Clone, Default)]
pub struct MakeCheckOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// A seam that runs the verification command. The default impl shells out to
/// make check in the configured directory; tests inject a closure that returns
/// canned output so no real subprocess is spawned.
type CommandRunner = Arc<dyn Fn() -> MakeCheckOutput + Send + Sync>;

/// A VerifyGate that runs make check in the workspace root and parses the
/// output for the failing step. The spawn routes through the ProcessLauncher
/// port so the spawn chokepoint (audit, fence) applies. The gate is injectable
/// for tests via with_runner (canned output) or with_launcher (a stub
/// launcher).
pub struct MakeCheckGate {
    /// Directory to run make check in (the workspace root).
    dir: PathBuf,
    /// Optional injection seam returning canned output. None in production;
    /// set in tests to avoid spawning a real subprocess.
    runner: Option<CommandRunner>,
    /// Optional launcher override. None in production defaults to the
    /// default launcher; set in tests to a stub that returns canned output.
    launcher: Option<Arc<dyn ProcessLauncher>>,
}

impl MakeCheckGate {
    /// Construct a gate that runs make check in the given directory.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            runner: None,
            launcher: None,
        }
    }

    /// Construct a gate for the workspace root (current dir of the process).
    pub fn workspace_root() -> Self {
        Self::new(".")
    }

    /// Inject a closure that returns canned make-check output, for tests. The
    /// closure replaces the real subprocess so no make is spawned.
    pub fn with_runner<F>(mut self, runner: F) -> Self
    where
        F: Fn() -> MakeCheckOutput + Send + Sync + 'static,
    {
        self.runner = Some(Arc::new(runner));
        self
    }

    /// Inject a launcher the gate uses to spawn make check. When None
    /// (the default), the gate uses the default launcher. Set in tests to
    /// a stub that returns canned output without spawning a real process.
    pub fn with_launcher(mut self, launcher: Arc<dyn ProcessLauncher>) -> Self {
        self.launcher = Some(launcher);
        self
    }

    /// Run make check and parse the output. Returns the captured output plus
    /// the parsed failure (None when the check passed).
    fn run_and_parse(&self) -> (MakeCheckOutput, Option<VerifyFailure>) {
        let out = match &self.runner {
            Some(f) => f(),
            None => run_make_check_via_launcher(self.dir.clone(), self.launcher.as_deref()),
        };
        if out.success {
            (out, None)
        } else {
            let failure = parse_failures(&out);
            (out, Some(failure))
        }
    }
}

impl VerifyGate for MakeCheckGate {
    fn verify(
        &self,
        _session: SessionId,
        _store: &dyn SessionLog,
    ) -> PFut<'_, Result<(), VerifyFailure>> {
        // Sync subprocess; there is no async surface to await. The session and
        // store are accepted by the trait so future gates (adversarial verify
        // that inspects the transcript) can use them; this gate only shells out.
        Box::pin(async move { self.verify_blocking() })
    }
}

impl MakeCheckGate {
    /// Blocking form used by the async verify. Separated so tests can call it
    /// directly without a tokio runtime.
    fn verify_blocking(&self) -> Result<(), VerifyFailure> {
        match self.run_and_parse() {
            (_, None) => Ok(()),
            (out, Some(mut failure)) => {
                // If parsing found no concrete step, fall back to a generic
                // entry so the caller still sees the failure.
                if failure.checks.is_empty() {
                    failure.push("make check exited non-zero".to_string());
                    failure
                        .suggestions
                        .push("inspect the full make check output".into());
                }
                drop(out);
                Err(failure)
            }
        }
    }
}

/// Shell out to make check in dir via the ProcessLauncher port, merging
/// stdout+stderr. Returns the captured output. Errors spawning the process
/// are reported as a failed check with the error text in stderr (so the gate
/// fails closed: a missing make is a failure, not a pass). When no launcher is
/// provided, the default launcher is used so the spawn chokepoint applies
/// even without explicit wiring.
fn run_make_check_via_launcher(
    dir: PathBuf,
    launcher: Option<&dyn ProcessLauncher>,
) -> MakeCheckOutput {
    let default_launcher = StdProcessLauncher::new();
    let launcher: &dyn ProcessLauncher = launcher.unwrap_or(&default_launcher);
    let req = SpawnRequest::new("make")
        .with_args(["check"])
        .with_workspace(dir)
        .piped_output();
    let policy = SpawnPolicy::default().audited();
    match launcher.spawn(req, policy) {
        Ok(child) => {
            let exit: Result<LauncherExit, _> = pollster::block_on(child.wait());
            match exit {
                Ok(e) => MakeCheckOutput {
                    success: e.exit_code == Some(0),
                    stdout: e.stdout.unwrap_or_default(),
                    stderr: e.stderr.unwrap_or_default(),
                },
                Err(e) => MakeCheckOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: format!("spawn failed: {e}"),
                },
            }
        }
        Err(e) => MakeCheckOutput {
            success: false,
            stdout: String::new(),
            stderr: format!("failed to spawn make check: {e}"),
        },
    }
}

/// Parse merged make-check output for the failing step(s). check_code.sh stops
/// at the first failure and prints a fail marker with the step name; we extract
/// that step name and summarize it. We also scan for common cargo markers
/// (error brackets, warning counts, FAILED test lines) to add detail.
fn parse_failures(out: &MakeCheckOutput) -> VerifyFailure {
    let merged = format!("{}\n{}", out.stdout, out.stderr);
    let mut failure = VerifyFailure::new();

    // The check_code.sh fail marker: a leading U+2717 then the step name then
    // FAILED. The marker is ANSI-colored in real output but the bare glyph
    // survives; strip ANSI then match on it.
    for line in merged.lines() {
        if let Some(step) = extract_failed_step(line) {
            failure.push(summarize_step(&step, &merged));
        }
    }

    // Direct cargo markers, useful when make isn't the entrypoint or the step
    // marker is missing. Only add when we have not already captured the step.
    if failure.checks.is_empty() {
        let errors = count_lines(&merged, "error[");
        let clippy_err = count_lines(&merged, "error: ");
        let test_fail = merged.lines().filter(|l| l.contains("... FAILED")).count();
        let fmt_diff = merged.lines().filter(|l| l.contains("Diff in")).count();
        if clippy_err > 0 || errors > 0 {
            failure.push(format!(
                "clippy or compiler: {} error lines",
                clippy_err.max(errors)
            ));
        }
        if test_fail > 0 {
            failure.push(format!("tests: {} failed", test_fail));
        }
        if fmt_diff > 0 {
            failure.push(format!("fmt-check: {} files need formatting", fmt_diff));
        }
    }

    // Naming/comments gates emit violation lines.
    if failure.checks.is_empty() {
        let naming = count_lines(&merged, "violation");
        if naming > 0 {
            failure.push(format!("naming: {} violation(s)", naming));
        }
    }

    if !failure.checks.is_empty() {
        failure
            .suggestions
            .push("fix the failing check then re-run".into());
    }
    failure
}

/// Extract the step name from a fail marker line (U+2717 then the step name
/// then FAILED), returning None when the line isn't a fail marker. Strips ANSI
/// escapes first: the real marker is wrapped in color codes.
fn extract_failed_step(line: &str) -> Option<String> {
    let clean = strip_ansi(line);
    let rest = clean.strip_prefix('\u{2717}')?; // U+2717 BALLOT X
    let rest = rest.trim();
    let name = rest.split_whitespace().next()?;
    if rest.contains("FAILED") {
        Some(name.to_string())
    } else {
        None
    }
}

/// Summarize one failing step with a short label. Keeps a stable vocabulary so
/// the caller can route failures (show in the TUI, re-prompt the model).
fn summarize_step(step: &str, merged: &str) -> String {
    match step {
        "fmt-check" => "fmt-check: formatting errors".to_string(),
        "clippy" => {
            let n = count_lines(merged, "error: ");
            format!("clippy -D warnings: {n} error(s)")
        }
        "comments" => "comments gate: banned tokens in .rs comments".to_string(),
        "naming" => {
            let n = count_lines(merged, "violation");
            format!("naming: {n} violation(s)")
        }
        "file-size" => "file-size gate: file over limit".to_string(),
        "typecheck" => {
            let n = count_lines(merged, "error[");
            format!("typecheck: {n} error(s)")
        }
        "test" => {
            let n = merged.lines().filter(|l| l.contains("... FAILED")).count();
            format!("tests: {n} failed")
        }
        other => format!("{other}: failed"),
    }
}

fn count_lines(haystack: &str, needle: &str) -> usize {
    haystack.lines().filter(|l| l.contains(needle)).count()
}

/// Strip a common subset of ANSI escape sequences so the fail glyph marker is
/// reachable. Only CSI sequences (ESC then bracket then one final byte
/// 0x40..0x7e) are removed; that covers the color codes check_code.sh emits.
/// Operates on chars (not bytes) so multi-byte UTF-8 like the fail glyph
/// survives intact.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        // ESC starts a CSI sequence: ESC [ ... (one char in 0x40..=0x7e).
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for final_byte in chars.by_ref() {
                if (0x40..=0x7e).contains(&(final_byte as u32)) {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{RunOutcome, Runner, RunnerConfig, ToolRegistry};
    use crate::provider::test_support::FakeProvider;
    use houyicoder_api::launcher::{LauncherChild, SpawnError};
    use houyicoder_api::provider::ModelProvider;
    use houyicoder_api::tool::{Tool, ToolCtx};
    use houyicoder_async::PFut;
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_protocol::extension::ToolError;
    use houyicoder_protocol::llm::Usage;
    use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
    use houyicoder_resilience::Retry;
    use houyicoder_session::SessionStore;

    fn out(success: bool, stdout: &str, stderr: &str) -> MakeCheckOutput {
        MakeCheckOutput {
            success,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    // ---- MakeCheckGate output parsing ----

    #[test]
    fn test_parse_clippy_marker() {
        let o = out(
            false,
            "\u{1b}[0;33m\u{25b6} Running clippy...\u{1b}[0m\n\
             error: unused variable\n\
             \n\
             \u{1b}[0;31m\u{2717} clippy FAILED \u{2014} stopping here.\u{1b}[0m\n",
            "",
        );
        let f = parse_failures(&o);
        assert!(f.checks.iter().any(|c| c.contains("clippy")));
        assert!(!f.suggestions.is_empty());
    }

    #[test]
    fn test_parse_naming() {
        let o = out(
            false,
            "naming: 2 violation(s)\n\u{2717} naming FAILED \u{2014} stopping here.\n",
            "",
        );
        let f = parse_failures(&o);
        assert!(f.checks.iter().any(|c| c.contains("naming")));
    }

    #[test]
    fn test_parse_test_marker() {
        let o = out(
            false,
            "test foo ... FAILED\n\u{2717} test FAILED \u{2014} stopping here.\n",
            "",
        );
        let f = parse_failures(&o);
        assert!(f.checks.iter().any(|c| c.contains("tests: 1 failed")));
    }

    #[test]
    fn test_parse_cargo_fallback() {
        // No check_code.sh marker (running cargo directly). The clippy error
        // line should still produce an entry.
        let o = out(false, "error: mismatched types\n", "");
        let f = parse_failures(&o);
        assert!(!f.checks.is_empty(), "should detect clippy/compiler errors");
    }

    #[test]
    fn test_parse_success() {
        let o = out(true, "all good\n", "");
        let f = parse_failures(&o);
        assert!(f.checks.is_empty());
    }

    // ---- MakeCheckGate blocking verify ----

    #[test]
    fn test_gate_ok() {
        let gate = MakeCheckGate::workspace_root().with_runner(|| out(true, "", ""));
        assert!(gate.verify_blocking().is_ok());
    }

    #[test]
    fn test_gate_fails() {
        let gate = MakeCheckGate::workspace_root().with_runner(|| {
            out(
                false,
                "error: unused\n\u{2717} clippy FAILED \u{2014} stopping here.\n",
                "",
            )
        });
        let err = gate.verify_blocking().expect_err("should fail");
        assert!(err.checks.iter().any(|c| c.contains("clippy")));
        assert!(!err.suggestions.is_empty());
    }

    #[test]
    fn test_gate_closed() {
        // Non-zero exit but no recognizable marker: still a failure with a
        // generic entry, never a silent pass.
        let gate = MakeCheckGate::workspace_root().with_runner(|| out(false, "boom\n", ""));
        let err = gate.verify_blocking().expect_err("should fail");
        assert!(!err.checks.is_empty(), "must report something");
    }

    #[tokio::test]
    async fn test_gate_async() {
        // The async trait method must agree with the blocking path.
        let gate = MakeCheckGate::workspace_root().with_runner(|| {
            out(
                false,
                "naming: 1 violation\n\u{2717} naming FAILED \u{2014} stopping here.\n",
                "",
            )
        });
        let store = SessionStore::new(Box::new(InMemoryBackend::new()));
        let err = gate
            .verify(SessionId::new(), &store)
            .await
            .expect_err("should fail");
        assert!(err.checks.iter().any(|c| c.contains("naming")));
    }

    #[test]
    fn test_strip_ansi() {
        let s = "\u{1b}[0;31m\u{2717} clippy FAILED\u{1b}[0m";
        assert_eq!(strip_ansi(s), "\u{2717} clippy FAILED");
    }

    // ---- Launcher-based gate ----

    /// A stub launcher that returns canned output, proving the gate routes
    /// through the ProcessLauncher port instead of a direct Command.
    struct StubLauncher {
        exit_code: i32,
        stdout: String,
        stderr: String,
    }

    impl ProcessLauncher for StubLauncher {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            let exit_code = self.exit_code;
            let stdout = self.stdout.clone();
            let stderr = self.stderr.clone();
            Ok(LauncherChild::new(
                None,
                Box::pin(async move {
                    Ok(LauncherExit {
                        exit_code: Some(exit_code),
                        stdout: Some(stdout),
                        stderr: Some(stderr),
                    })
                }),
            ))
        }
    }

    #[test]
    fn test_gate_via_launcher_ok() {
        let launcher = Arc::new(StubLauncher {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        });
        let gate = MakeCheckGate::workspace_root().with_launcher(launcher);
        assert!(gate.verify_blocking().is_ok());
    }

    #[test]
    fn test_gate_via_launcher_fail() {
        let launcher = Arc::new(StubLauncher {
            exit_code: 1,
            stdout: String::from("error: unused\n\u{2717} clippy FAILED \u{2014} stopping here.\n"),
            stderr: String::new(),
        });
        let gate = MakeCheckGate::workspace_root().with_launcher(launcher);
        let err = gate.verify_blocking().expect_err("should fail");
        assert!(err.checks.iter().any(|c| c.contains("clippy")));
    }

    #[test]
    fn test_gate_via_launcher_closed() {
        let launcher = Arc::new(StubLauncher {
            exit_code: 1,
            stdout: String::from("boom\n"),
            stderr: String::new(),
        });
        let gate = MakeCheckGate::workspace_root().with_launcher(launcher);
        let err = gate.verify_blocking().expect_err("should fail");
        assert!(!err.checks.is_empty(), "must report something");
    }

    // ---- Runner verify-gate wiring ----

    /// A test-only VerifyGate with a fixed result, so the runner wiring can be
    /// exercised without spawning make. fail holds the outcome to return.
    struct StubVerifyGate {
        fail: Option<VerifyFailure>,
    }
    impl VerifyGate for StubVerifyGate {
        fn verify(
            &self,
            _session: SessionId,
            _store: &dyn SessionLog,
        ) -> PFut<'_, Result<(), VerifyFailure>> {
            let fail = self.fail.clone();
            Box::pin(async move {
                match fail {
                    None => Ok(()),
                    Some(f) => Err(f),
                }
            })
        }
    }

    fn runner_with(provider: Arc<dyn ModelProvider>) -> Runner {
        runner_with_tools(provider, ToolRegistry::new())
    }

    fn runner_with_tools(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Runner {
        Runner::new(
            std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())))
                as std::sync::Arc<dyn houyicoder_api::session::SessionLog>,
            provider,
            tools,
            RunnerConfig {
                model: "test".into(),
                instructions: "you are a test agent".into(),
                max_turns: 5,
                max_output_tokens: 8_000,
                retry: Retry {
                    max_attempts: 2,
                    ..Retry::default()
                },
            },
        )
    }

    /// A tool that requires approval, exercising the Interruption path.
    struct GuardedTool;
    impl Tool for GuardedTool {
        fn name(&self) -> &str {
            "guarded"
        }
        fn description(&self) -> &str {
            "a tool that needs human approval"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn execute(
            &self,
            _ctx: ToolCtx,
            input: serde_json::Value,
        ) -> PFut<'_, Result<serde_json::Value, ToolError>> {
            Box::pin(async move { Ok(serde_json::json!({"ran": input})) })
        }
        fn requires_approval(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn test_runner_gate_ok() {
        // Gate returns Ok so the run surfaces FinalOutput unchanged.
        let p = Arc::new(FakeProvider::text("done"));
        let gate = Arc::new(StubVerifyGate { fail: None });
        let runner = runner_with(p).with_verify_gate(gate);
        let session = SessionId::new();
        let result = runner.run(session, "hi".into()).await.unwrap();
        match result.outcome {
            RunOutcome::FinalOutput(t) => assert_eq!(t, "done"),
            other => panic!("expected final output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_runner_gate_fail() {
        // Gate returns Err so FinalOutput is replaced with VerifyFailed
        // carrying the gate findings, so the caller can re-prompt the model.
        let p = Arc::new(FakeProvider::text("done"));
        let gate = Arc::new(StubVerifyGate {
            fail: Some(VerifyFailure {
                checks: vec!["clippy -D warnings: 2 errors".into()],
                suggestions: vec!["run cargo clippy --fix".into()],
            }),
        });
        let runner = runner_with(p).with_verify_gate(gate);
        let session = SessionId::new();
        let result = runner.run(session, "hi".into()).await.unwrap();
        match result.outcome {
            RunOutcome::VerifyFailed(f) => {
                assert!(f.checks.iter().any(|c| c.contains("clippy")));
                assert!(!f.suggestions.is_empty());
            }
            other => panic!("expected verify failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_runner_no_gate() {
        // No gate installed means FinalOutput passes through (backward compat).
        let p = Arc::new(FakeProvider::text("done"));
        let runner = runner_with(p);
        let session = SessionId::new();
        let result = runner.run(session, "hi".into()).await.unwrap();
        assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    }

    #[tokio::test]
    async fn test_runner_gate_skips_interrupt() {
        // Verify only fires on FinalOutput, not Interruption — the caller must
        // resolve approvals first. A failing gate on an interrupted run would
        // wrongly shadow the approval flow.
        let resp = CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "guarded".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        };
        let p = Arc::new(FakeProvider::new(vec![resp]));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(GuardedTool));
        let gate = Arc::new(StubVerifyGate {
            fail: Some(VerifyFailure {
                checks: vec!["should not run".into()],
                suggestions: vec![],
            }),
        });
        let runner = runner_with_tools(p, tools).with_verify_gate(gate);
        let session = SessionId::new();
        let result = runner.run(session, "hi".into()).await.unwrap();
        match result.outcome {
            RunOutcome::Interruption(a) => assert_eq!(a.len(), 1),
            other => panic!("expected interruption, got {other:?}"),
        }
    }
}
