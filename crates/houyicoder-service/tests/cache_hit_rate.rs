//! Live steady-state prompt-cache measurement: one endpoint, one ratio,
//! printed. Ignored because every call spends real tokens against the
//! configured endpoint.
//!
//! The measured group is the pre-registered one: a single endpoint, a single
//! model, a single workspace, and turns whose request only appends a short tail
//! to the request before it. Each repeat opens a fresh session, so its first
//! call writes the prefix the later calls reuse and the rest of the repeat is
//! the steady state the ratio is judged over.
//!
//! That ratio is the sum of cache-read input divided by the sum of inclusive
//! input. A call enters the sum only when the endpoint reported both counts:
//! a call whose endpoint said nothing about its cache read is unknown, and
//! folding it in as a miss is the one way to make a hit rate read as something
//! that did not happen.
//!
//! What this measures is the endpoint's side of the two failure modes: the
//! request bytes are proven to extend the previous request by the offline
//! reuse check in the prefix reuse suite. Run it with the config home pointed
//! at a settings file, so the model under measurement is named rather than
//! inherited from the default:
//!
//!     HOUYICODER_CONFIG_HOME=<dir> make benchmark cache

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::path::PathBuf;

use houyicoder_context::SessionEvent;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_protocol::llm::UsageProvenance;
use houyicoder_service::composition::AssembledRunner;
use houyicoder_service::composition::BuildRunnerOptions;
use houyicoder_service::composition::build_runner;

/// Sessions measured. The pre-registered group requires at least three
/// independent repeats.
const REPEATS: usize = 3;

/// Turns per session. The first call writes the prefix, so a repeat yields one
/// prefix-building call plus the steady state after it.
const TURNS: usize = 12;

/// The pre-registered minimum of measured calls, so a run that recorded
/// nothing says so instead of printing a ratio over one or two calls.
const MIN_MEASURED_CALLS: usize = 10;

/// One endpoint call, as the durable log recorded it.
struct Call {
    turn: u32,
    input: u64,
    cached: u64,
    reported: bool,
}

/// One workspace for every repeat: a stable directory, so the system prompt,
/// the tool list, and the project rules are the same bytes each time and the
/// request differs only by the tail.
fn workspace() -> PathBuf {
    let dir = temp_dir().join("houyi-cache-hit-rate");
    create_dir_all(&dir).expect("measurement workspace");
    dir
}

/// The calls one session recorded, in order. Read from the log rather than from
/// the cumulative tally: the log keeps one row per endpoint call, so a turn
/// that re-called contributes each call rather than their sum alone.
fn session_calls(runner: &Runner, session: SessionId) -> Vec<Call> {
    runner
        .trajectory_snapshot(session)
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::TurnUsage {
                turn,
                input_tokens,
                cache_read_input_tokens,
                input_reported,
                cache_read_reported,
                cache_write_reported,
                ..
            } => Some(Call {
                turn,
                input: input_tokens,
                cached: cache_read_input_tokens,
                reported: UsageProvenance {
                    input_reported,
                    cache_read_reported,
                    cache_write_reported,
                }
                .cache_account_complete(),
            }),
            _ => None,
        })
        .collect()
}

/// A runner over the measurement workspace, assembled by the production root so
/// every request is built the way the product builds it.
fn open(project: &str) -> AssembledRunner {
    let mut options = BuildRunnerOptions::default();
    options.project = Some(project.to_string());
    build_runner(options)
}

/// Drive one session and return the calls it recorded. The prompts ask for a
/// bare word: the measurement needs the request to grow by a tail, not to make
/// the model reach for a tool.
async fn run_repeat(assembled: &AssembledRunner) -> Vec<Call> {
    let runner = &assembled.runner;
    let session = assembled.session;
    for turn in 1..=TURNS {
        let prompt = format!("Reply with the single word ack. This is message {turn}.");
        runner
            .run(session, prompt)
            .await
            .unwrap_or_else(|e| panic!("repeat turn {turn} failed: {e:?}"));
    }
    session_calls(runner, session)
}

/// Print the per-call rows and the group's ratio, then return the ratio and
/// how many calls entered it.
fn report(all: &[Vec<Call>]) -> (f64, usize) {
    let mut cached = 0u64;
    let mut input = 0u64;
    let mut measured = 0usize;
    let mut excluded = 0usize;
    println!("group: {} repeats, {TURNS} turns each", all.len());
    for (index, calls) in all.iter().enumerate() {
        let first = calls.first();
        println!(
            "repeat {}: {} calls; first call input {} cached {}",
            index + 1,
            calls.len(),
            first.map_or(0, |c| c.input),
            first.map_or(0, |c| c.cached)
        );
        for call in calls.iter().skip(1) {
            if !call.reported {
                excluded += 1;
                println!(
                    "  turn {:<3} excluded: the endpoint reported no cache account",
                    call.turn
                );
                continue;
            }
            measured += 1;
            cached += call.cached;
            input += call.input;
            let share = if call.input == 0 {
                0.0
            } else {
                call.cached as f64 * 100.0 / call.input as f64
            };
            println!(
                "  turn {:<3} input {:<7} cached {:<7} share {share:.1}%",
                call.turn, call.input, call.cached
            );
        }
    }
    let ratio = if input == 0 {
        0.0
    } else {
        cached as f64 * 100.0 / input as f64
    };
    println!(
        "steady state: {cached} cache-read / {input} inclusive input = {ratio:.2}% over \
         {measured} calls ({excluded} excluded)"
    );
    (ratio, measured)
}

#[tokio::test]
#[ignore = "calls a real endpoint; set HOUYICODER_CONFIG_HOME to the settings that name the model"]
async fn test_steady_state_hit_rate() {
    let project = workspace();
    let project = project.display().to_string();
    let mut all = Vec::new();
    for index in 0..REPEATS {
        let assembled = open(&project);
        if index == 0 {
            let facts = &assembled.provider_facts;
            println!(
                "endpoint {} key {} model {}",
                facts.base_url,
                facts.key_origin.as_deref().unwrap_or("none"),
                assembled.runner.status_snapshot().model
            );
            // A run with no key answers from the canned stub, which reports no
            // cache account at all. Measuring that would print a ratio about
            // the stub, so refuse it instead of reporting it.
            assert!(
                facts.key_origin.is_some(),
                "no credential resolved for {}; a stubbed run is not a measurement",
                facts.base_url
            );
        }
        all.push(run_repeat(&assembled).await);
    }
    let (ratio, measured) = report(&all);
    assert!(
        measured >= MIN_MEASURED_CALLS,
        "the group needs at least {MIN_MEASURED_CALLS} measured calls, got {measured}"
    );
    assert!(
        (0.0..=100.0).contains(&ratio),
        "a share of the input cannot fall outside 0..100"
    );
}
