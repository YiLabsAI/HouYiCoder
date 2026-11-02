//! Live benchmark for reward-loop learning and recall. The benchmark dispatcher
//! runs paired queries with reward enabled and disabled, then prints comparable
//! tool, error, redundancy, and memory metrics. Requires a real provider.

use houyicoder_context::SessionEvent;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_service::composition::BuildRunnerOptions;
use houyicoder_service::composition::build_runner;
use std::env::temp_dir;
use std::env::var;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::process::id;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

struct QueryMetrics {
    redundant: usize,
    calls: u32,
    errors: u32,
    recalled: u32,
    memory_keys: usize,
}

async fn run_query(runner: &Runner, session: SessionId, prompt: &str) -> QueryMetrics {
    let _run = runner.run(session, prompt.to_string()).await;
    let snap = runner.status_snapshot();
    let memory_keys = runner
        .format_memory_index()
        .map(|s| s.lines().count())
        .unwrap_or(0);
    QueryMetrics {
        redundant: runner.redundancy_snapshot().len(),
        calls: snap.tool_calls,
        errors: snap.tool_errors,
        recalled: runner
            .store()
            .trajectory_tail(session)
            .iter()
            .filter_map(|ev| match &ev.event {
                SessionEvent::MemoryRecall { keys, .. } => Some(keys.len() as u32),
                _ => None,
            })
            .sum(),
        memory_keys,
    }
}

/// Count memory entries whose origin is dream — the ones only the dream
/// fork could have written (extractor writes land as origin=extractor).
fn dream_count(runner: &Runner) -> usize {
    runner
        .format_memory_index()
        .map(|s| s.lines().filter(|l| l.contains("/dream]")).count())
        .unwrap_or(0)
}

/// A real cargo package whose add returns subtraction, so every test
/// file fails. query 1 re-runs each test binary to check for flakiness
/// (blind retries), producing retry_after_error >= 2, the signal that
/// lets the dream pass run.
fn write_fail_project(repo: &Path) {
    create_dir_all(repo.join("src")).expect("mkdir src");
    create_dir_all(repo.join("tests")).expect("mkdir tests");
    write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"failing_proj\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .expect("manifest");
    write(
        repo.join("src").join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a - b }\n",
    )
    .expect("lib");
    let test_body = "use failing_proj::add;\n#[test] fn checks() { assert_eq!(add(2, 3), 5); }\n";
    write(repo.join("tests").join("a.rs"), test_body).expect("a");
    write(repo.join("tests").join("b.rs"), test_body).expect("b");
    write(repo.join("tests").join("c.rs"), test_body).expect("c");
}

async fn run_pair() -> (QueryMetrics, bool, QueryMetrics) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = temp_dir().join(format!("houyi-verify-{}-{n}", id()));
    let _prev = remove_dir_all(&root);
    // The memory slug is derived from the repo dir name (git_canonical_slug
    // falls back to the canonical dir name for a non-git dir). The name says
    // what the directory holds — a project whose tests fail — and the pid
    // keeps each run's auto-memory entries separate from the last.
    let repo_name = format!("failing-project-{}", id());
    let repo = root.join(&repo_name);
    write_fail_project(&repo);
    let mut options = BuildRunnerOptions::default();
    options.project = Some(repo.to_string_lossy().into_owned());
    // The auto-memory root resolves under this home, so the dream lessons and
    // the memory index stay inside the test's temp root.
    options.home = Some(root.join("home"));
    let bundle = build_runner(options);
    let runner = bundle.runner;
    let session1 = bundle.session;
    let baseline_dream = dream_count(&runner);
    let fail_metrics = run_query(&runner, session1, PROMPT_FAIL).await;
    // query 1's FinalOutput starts the dream — the background pass that
    // distills lessons from the run (retry_after_error>=2 clears its gate on
    // the first query since last_scan=0). Wait for a dream-origin lesson to
    // land; the origin distinguishes those writes from the extractor's.
    // Await the pass's JoinHandle directly (event-driven, no sleep poll).
    // Skip when reward is off, since the dream will not run.
    if var("HOUYICODER_REWARD_OFF").is_err() {
        runner.join_dreams(Duration::from_secs(90)).await;
    }
    let dream_wrote = dream_count(&runner) > baseline_dream;
    let session2 = SessionId::new();
    let recall_metrics = run_query(&runner, session2, PROMPT_RECALL).await;
    // Dump the concrete redundant calls + memory keys so a human can tell
    // real duplicates from a tracker misfire (hash collision, missed write).
    for rc in runner.redundancy_snapshot() {
        eprintln!(
            "redundant: tool={} kind={:?} gap={} preview={}",
            rc.tool, rc.kind, rc.gap, rc.input_preview
        );
    }
    if let Some(idx) = runner.format_memory_index() {
        eprintln!("memory index after recall query:\n{}", idx);
    }
    // The injected home sits inside the temp root, so this removes the
    // lessons and the memory index this run produced and nothing else.
    let _cleanup = remove_dir_all(&root);
    (fail_metrics, dream_wrote, recall_metrics)
}

const PROMPT_FAIL: &str = "This Rust project has failing tests. Run `cargo test --test a` twice in a row to check if the failure is flaky, then run `cargo test --test b` twice, then `cargo test --test c` twice. Do not edit any files between runs. Report each result.";

const PROMPT_RECALL: &str = "The project still has failing tests. Run `cargo test --test a` and try to fix the bug in src/lib.rs so it passes.";

#[tokio::test]
#[ignore = "real provider, reward on"]
async fn test_reward_on_pair() {
    let (fail_metrics, dream_wrote, recall_metrics) = run_pair().await;
    println!(
        "reward ON  : fail errors={} calls={} redundant={} recalled={} mem={} | dream_wrote={} | recall errors={} calls={} redundant={} recalled={}",
        fail_metrics.errors,
        fail_metrics.calls,
        fail_metrics.redundant,
        fail_metrics.recalled,
        fail_metrics.memory_keys,
        dream_wrote,
        recall_metrics.errors,
        recall_metrics.calls,
        recall_metrics.redundant,
        recall_metrics.recalled
    );
}

#[tokio::test]
#[ignore = "real provider, reward off (set HOUYICODER_REWARD_OFF=1)"]
async fn test_reward_off_pair() {
    let (fail_metrics, dream_wrote, recall_metrics) = run_pair().await;
    println!(
        "reward OFF : fail errors={} calls={} redundant={} recalled={} mem={} | dream_wrote={} | recall errors={} calls={} redundant={} recalled={}",
        fail_metrics.errors,
        fail_metrics.calls,
        fail_metrics.redundant,
        fail_metrics.recalled,
        fail_metrics.memory_keys,
        dream_wrote,
        recall_metrics.errors,
        recall_metrics.calls,
        recall_metrics.redundant,
        recall_metrics.recalled
    );
}
