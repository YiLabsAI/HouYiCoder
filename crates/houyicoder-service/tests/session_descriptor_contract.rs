//! A session's task statement reaches its descriptor through the production
//! assembly, not only through a runner a unit test builds by hand. The test
//! drives one turn through build_runner with the disk preset at a temp root
//! and reads the descriptor back from that root.

use houyicoder_context::{PromptCoverage, SessionDescriptorStore};
use houyicoder_memory::FileDescriptorStore;
use houyicoder_provider::ProviderResolution;
use houyicoder_service::composition::BuildRunnerOptions;
use houyicoder_service::composition::build_runner;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::process::id;

/// One turn through the assembled runner records the prompt with the event it
/// came from.
#[tokio::test]
async fn test_run_records_prompt() {
    let root = temp_dir().join(format!("houyi-intent-{}-{}", id(), line!()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("mkdir root");
    let mut options = BuildRunnerOptions::disk_at(root.clone(), None, None);
    options.home = Some(root.join("home"));
    options.provider = Some(ProviderResolution::stub());
    let bundle = build_runner(options);
    bundle.runner.set_auto_memory(false);
    bundle.runner.set_auto_dream(false);
    let session = bundle.session;

    bundle
        .runner
        .run(session, "fix the login test".to_string())
        .await
        .expect("the turn runs");
    // The prompt write runs detached from the turn, so the read below waits
    // for it instead of racing it.
    bundle.runner.settle_prompt_writes().await;

    let store = FileDescriptorStore::new(root.clone());
    let descriptor = store
        .read_descriptor(session)
        .expect("the descriptor is on disk");
    let first = descriptor
        .first_prompt
        .expect("the first prompt is recorded");
    assert_eq!(first.text, "fix the login test");
    assert_eq!(
        descriptor
            .last_prompt
            .expect("the last prompt follows")
            .text,
        "fix the login test"
    );
    assert_eq!(descriptor.prompt_coverage, PromptCoverage::Complete);
    let log = read_to_string(root.join(session.to_string()).join("log.jsonl")).expect("log");
    assert!(
        log.contains(&first.event_id.to_string()),
        "the record names an event the log holds"
    );
    drop(bundle);
    drop(remove_dir_all(&root));
}
