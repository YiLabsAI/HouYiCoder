//! Self-evolution reward-loop benchmark. Two ignored tests run a query
//! pair against one Runner so the cross-query dream trigger and recall
//! path get exercised. Set HOUYICODER_REWARD_OFF=1 for the off variant.
//! Needs a real provider. Not in make verify — run manually via
//! cargo test --test reward_bench -- --ignored.

#![cfg(test)]

use houyicoder_context::SessionId;

struct QueryMetrics {
    redundant: usize,
    calls: u32,
    errors: u32,
    recalled: u32,
    memory_keys: usize,
}

async fn run_query(
    runner: &houyicoder_core::agent::Runner,
    session: SessionId,
    prompt: &str,
) -> QueryMetrics {
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
            .trajectory_snapshot(session)
            .iter()
            .filter_map(|ev| match &ev.kind {
                houyicoder_context::TurnEventKind::MemoryRecall { keys, .. } => {
                    Some(keys.len() as u32)
                }
                _ => None,
            })
            .sum(),
        memory_keys,
    }
}

/// Count memory entries whose origin is dream — the ones only the dream
/// fork could have written (extractor writes land as origin=extractor).
fn dream_count(runner: &houyicoder_core::agent::Runner) -> usize {
    runner
        .format_memory_index()
        .map(|s| s.lines().filter(|l| l.contains("/dream]")).count())
        .unwrap_or(0)
}

/// A real cargo package whose add returns subtraction, so every test
/// file fails. query 1 re-runs each test binary to check for flakiness
/// (blind retries), producing retry_after_error >= 2 to clear the
/// reward-dream gate.
fn write_fail_project(repo: &std::path::Path) {
    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    std::fs::create_dir_all(repo.join("tests")).expect("mkdir tests");
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"smoke_proj\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .expect("manifest");
    std::fs::write(
        repo.join("src").join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a - b }\n",
    )
    .expect("lib");
    let test_body = "use smoke_proj::add;\n#[test] fn checks() { assert_eq!(add(2, 3), 5); }\n";
    std::fs::write(repo.join("tests").join("a.rs"), test_body).expect("a");
    std::fs::write(repo.join("tests").join("b.rs"), test_body).expect("b");
    std::fs::write(repo.join("tests").join("c.rs"), test_body).expect("c");
}

async fn run_pair() -> (QueryMetrics, bool, QueryMetrics) {
    let root = std::env::temp_dir().join(format!("houyi-verify-{}", std::process::id()));
    let _prev = std::fs::remove_dir_all(&root);
    // The memory slug is derived from the repo dir name (git_canonical_slug
    // falls back to the canonical dir name for a non-git dir). A PID-unique
    // name makes the slug test-specific: a real project named "repo" is not
    // nuked, and a prior run's dream lessons do not leak (different PID).
    let repo_name = format!("smoke-{}", std::process::id());
    let repo = root.join(&repo_name);
    write_fail_project(&repo);
    let bundle = houyicoder_service::composition::build_runner(
        Some(repo.to_string_lossy().into_owned()),
        None,
        None,
    );
    let runner = bundle.runner;
    let session1 = bundle.session;
    let baseline_dream = dream_count(&runner);
    let fail_metrics = run_query(&runner, session1, PROMPT_FAIL).await;
    // query 1 FinalOutput fires reward-dream (retry_after_error>=2 clears
    // the gate on the first query since last_scan=0). Wait for a dream-origin
    // lesson to land — origin distinguishes dream from extractor writes.
    // Await the dream's JoinHandle directly (event-driven, no sleep poll).
    // Skip when reward is off (the dream will not fire).
    if std::env::var("HOUYICODER_REWARD_OFF").is_err() {
        runner.join_dreams(std::time::Duration::from_secs(90)).await;
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
    // Clean up the memory dir the runner wrote under HOME. The slug is the
    // repo dir name, so this targets only this run's memory — not a real
    // project's.
    if let Ok(home) = std::env::var("HOME") {
        let mem = std::path::Path::new(&home)
            .join(".houyicoder")
            .join("projects")
            .join(&repo_name)
            .join("memory");
        let _mem = std::fs::remove_dir_all(&mem);
    }
    let _cleanup = std::fs::remove_dir_all(&root);
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
