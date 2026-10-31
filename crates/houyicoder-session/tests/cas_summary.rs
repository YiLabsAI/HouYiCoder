//! The stored-block account a session reports from its durable log: the
//! figures the fold states, the ones it leaves unknown, and a replayed
//! assembly that is counted once.

use houyicoder_context::{
    CachedModelView, CachedRead, EventId, ReadActor, ReadOutcome, SessionEvent, SessionId,
    SessionLogEntry, ViewForm,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;
use houyicoder_protocol::capture_record::{
    CAPTURE_KEY, RECORD_KEY, STDERR_BYTES_KEY, STDOUT_BYTES_KEY,
};
use houyicoder_session::SessionStore;
use serde_json::json;

fn store() -> SessionStore {
    SessionStore::new(Box::new(InMemoryBackend::new()))
}

fn entry(session: SessionId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event,
    }
}

fn view(block_ref: &str, form: ViewForm, size: u64) -> CachedModelView {
    CachedModelView {
        block_ref: block_ref.to_string(),
        form,
        size,
    }
}

/// One session whose log names a stored result, an assembly, and a read
/// reports each figure the account can state, without a runner in the way.
#[tokio::test]
async fn test_cas_summary_folds_log() {
    let store = store();
    let session = SessionId::new();
    store
        .append(entry(
            session,
            SessionEvent::TurnUsage {
                turn: 1,
                call_in_turn: 1,
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: false,
                effort: None,
                input_reported: true,
                cache_read_reported: true,
                cache_write_reported: true,
                cas_recorded: true,
            },
        ))
        .await
        .expect("the usage is logged");
    store
        .append(entry(
            session,
            SessionEvent::ToolResult {
                call_id: "call-1".to_string(),
                output: json!({
                    CAPTURE_KEY: {
                        RECORD_KEY: { BLOCK_REF_KEY: "block-a" },
                        STDOUT_BYTES_KEY: 100,
                        STDERR_BYTES_KEY: 20,
                    }
                }),
                duration_ms: 3,
            },
        ))
        .await
        .expect("the result is logged");
    store
        .append(entry(
            session,
            SessionEvent::CasModelView {
                assembly_id: 1,
                views: vec![
                    view("block-a", ViewForm::Materialize, 10),
                    view("block-b", ViewForm::Evict, 2),
                ],
                estimate_unit: "tokens".to_string(),
            },
        ))
        .await
        .expect("the view is logged");
    store
        .append(entry(
            session,
            SessionEvent::CasRead {
                reads: vec![CachedRead {
                    result_id: "call-1:stdout".to_string(),
                    actor: ReadActor::Model,
                    start: 0,
                    bytes: 40,
                    outcome: ReadOutcome::Read,
                }],
            },
        ))
        .await
        .expect("the read is logged");

    let cas = store.trajectory_head(session).summary.cas;
    assert!(cas.tracked, "the log says its writer records the store");
    assert_eq!(cas.results, Some(1), "one capture published");
    assert_eq!(cas.blocks, 1, "the result addresses one block");
    assert_eq!(cas.raw_bytes, Some(120));
    assert_eq!(cas.unmeasured_results, Some(0));
    let assembly = cas.assembly.expect("the assembly is reported");
    assert_eq!(assembly.assembly_id, 1);
    assert_eq!(assembly.full, 1);
    assert_eq!(assembly.pointer, 1);
    assert_eq!(assembly.size, 12);
    assert_eq!(cas.model_reads, 1);
    assert!(cas.reads_recorded, "the log carries a read record");
}

/// A log written before the writer reported the store states no result count,
/// so the account reads unknown instead of claiming the session stored
/// nothing.
#[tokio::test]
async fn test_untracked_log_reports_unknown() {
    let store = store();
    let session = SessionId::new();
    store
        .append(entry(
            session,
            SessionEvent::ToolResult {
                call_id: "call-1".to_string(),
                output: json!({ BLOCK_REF_KEY: "block-a" }),
                duration_ms: 3,
            },
        ))
        .await
        .expect("the result is logged");

    let cas = store.trajectory_head(session).summary.cas;
    assert!(!cas.tracked);
    assert_eq!(cas.results, None, "an old log states no count");
    assert_eq!(cas.blocks, 1, "the marker still names a block");
    assert_eq!(cas.assembly, None, "no assembly was recorded");
    assert!(!cas.reads_recorded, "and no read record was written");
}

/// The same assembly written twice is counted from its later record, so a
/// replay does not double the size the account reports.
#[tokio::test]
async fn test_replayed_assembly_not_doubled() {
    let store = store();
    let session = SessionId::new();
    for views in [
        vec![
            view("block-a", ViewForm::Materialize, 10),
            view("block-b", ViewForm::Summarize, 5),
        ],
        vec![view("block-a", ViewForm::Materialize, 4)],
    ] {
        store
            .append(entry(
                session,
                SessionEvent::CasModelView {
                    assembly_id: 5,
                    views,
                    estimate_unit: "tokens".to_string(),
                },
            ))
            .await
            .expect("the view is logged");
    }

    let assembly = store
        .trajectory_head(session)
        .summary
        .cas
        .assembly
        .expect("the assembly is reported");
    assert_eq!(assembly.full, 1);
    assert_eq!(assembly.preview, 0, "the earlier record is replaced");
    assert_eq!(assembly.size, 4, "counted once, not twice");
}
