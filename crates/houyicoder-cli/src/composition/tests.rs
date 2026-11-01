//! Tests for composition assembly: assembly smoke, server/client handshake,
//! and bus event consumption through the real assembly.

use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::{Path, PathBuf};
use std::process::id as process_id;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use houyicoder_api::agent_event::{EventHandler, RunCompletionStatus, RunLifecycleEvent};
use houyicoder_async::bus::MessageBus;
use houyicoder_client::Client;
use houyicoder_context::{
    DescriptorUpdate, SessionDescriptor, SessionDescriptorError, SessionDescriptorStore, SessionId,
};
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, ChildDescriptor, ChildRunMode, global_completed_topic,
    global_progress_topic, spawned_topic,
};
use houyicoder_core::agent::multi_agent::status_publisher::ChildStatusPublisher;
use houyicoder_memory::InMemoryDescriptorStore;
use houyicoder_protocol::envelope::{
    ClientResponsePayload, RequestId, ResponsePayload, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::completion::CompletionStatus;
use houyicoder_protocol::frontend::event::FrontendEvent;
use houyicoder_protocol::frontend::trust::TrustAccept;
use houyicoder_service::composition::host_lifecycle::HostLifecycle;
use houyicoder_service::composition::{BuildRunnerOptions, ProviderResolution, build_runner};
use tokio::runtime::{Builder, Runtime};
use tokio::time::{sleep, timeout};

use super::*;
use crate::local_host::{HostLease, HostLeaseScope, HostLeaseSlot, release_all};
use crate::session::lock::SessionLock;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("composition-host-{}-{n}", process_id()));
    create_dir_all(&p).expect("temp root");
    p
}

/// Build one live session through the real composition. build_runner
/// always provides a real bus, so start_local_server starts the serve,
/// fleet relay, notification drain, and refresh tasks. The bus comes back
/// alongside so a test can publish on the exact bus the assembly
/// subscribed to. Dropping the client lets the serve exit -- keeping it
/// parks the serve.
fn start_host(
    root: &Path,
) -> (
    Arc<Runner>,
    Client,
    Weak<Runner>,
    HostLifecycle,
    SessionLock,
    Arc<AgentBus>,
) {
    let settings_path = root.join("settings.json");
    let mut options = BuildRunnerOptions::default();
    options.provider = Some(ProviderResolution::stub());
    let bundle = build_runner(options);
    let sid = bundle.session.to_string();
    let bus = bundle.bus.clone().expect("the real assembly carries a bus");
    let (runner, client, _warnings, host) =
        start_local_server(bundle, Some(settings_path), root.to_path_buf());
    let weak = Arc::downgrade(&runner);
    let lock = SessionLock::acquire(&sid, root).expect("acquire session lock");
    (runner, client, weak, host, lock, bus)
}

/// The real bus means the host tracks five tasks -- serve, fleet relay,
/// notification drain, model refresh, and the prompt backfill. Shutdown joins
/// the ones that finish within its budget; the backfill is still on its first
/// pass and takes the shutdown signal rather than running on. A bus-less
/// fixture would track only the serve, refresh, and backfill, so the relay and
/// drain rows prove they really ran, not that a stand-in exists.
#[test]
fn test_bus_tasks_joined() {
    let root = temp_root();
    let (runner, client, _weak, host, _lock, _bus) = start_host(&root);
    drop(client);
    drop(runner);
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let report = rt.block_on(host.begin_shutdown());
    assert_eq!(report.panic_count(), 0, "no task panicked during shutdown");
    assert_eq!(
        report.outcomes().len(),
        5,
        "serve, relay, drain, refresh, and backfill all tracked"
    );
    remove_dir_all(&root).ok();
}

/// Read frames until one agent-status frame for the probe matches want,
/// answering a startup trust prompt if the workspace was never trusted so
/// the test runs on a trusted machine and an untrusted one alike. Every read
/// is bounded, so a missing status fails the test instead of hanging it.
/// Statuses are read one transition at a time: the sequencer keeps only the
/// latest status per agent, so publishing the next transition before the
/// current one is read would coalesce them and drop the intermediate frame.
/// Reading each frame first is the latch that proves the relay flushed it.
async fn read_probe_status(
    client: &mut Client,
    probe_id: &str,
    want: impl Fn(u32, u64, Option<&CompletionStatus>, Option<&str>) -> bool,
) {
    loop {
        let frame = timeout(Duration::from_secs(5), client.next_frame())
            .await
            .expect("probe status frame timed out")
            .expect("frame read");
        match frame {
            ServerFrame::Request(req) => {
                if matches!(req.payload, ServerRequestPayload::TrustPrompt(_)) {
                    client
                        .send_reverse_response(
                            req.req_id,
                            ClientResponsePayload::TrustAccept(TrustAccept { accepted: true }),
                        )
                        .await
                        .expect("trust answer sent");
                }
            }
            ServerFrame::Event(env) => {
                if let FrontendEvent::AgentStatus {
                    agent_id,
                    turn,
                    tokens,
                    completed,
                    last_activity,
                    ..
                } = &env.payload
                {
                    assert_eq!(agent_id, probe_id, "the probe is the only fleet child");
                    if want(*turn, *tokens, completed.as_ref(), last_activity.as_deref()) {
                        return;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Poll the parent's notification queue until the drain's delivery for the
/// probe lands. The queue entry is the drain's effect, so polling it is
/// the latch: no fixed sleep standing in for an ordering claim.
async fn await_probe_notification(runner: &Runner, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if runner
            .queued_notifications_snapshot()
            .iter()
            .any(|text| text.contains(marker))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the drain never delivered {marker}"
        );
        sleep(Duration::from_millis(1)).await;
    }
}

/// Consumption on the real assembly: identifiable status events published
/// through the production emitter reach the connected client as agent
/// status frames (the relay's translation) and the parent's notification
/// queue (the drain's delivery), including the failure shape. Counting
/// tracked tasks cannot show this -- a relay that never consumed would
/// still be tracked. After shutdown the subscription footprint is gone on
/// every topic, which attributes the exit per consumer without task
/// positions: the spawn topic had exactly one subscriber (the relay) and
/// completion had two (relay and drain), so zero receivers now means both
/// ended.
#[test]
fn test_bus_events_consumed() {
    let root = temp_root();
    let (runner, mut client, _weak, host, _lock, bus) = start_host(&root);
    assert_eq!(
        bus.receiver_count(spawned_topic()),
        1,
        "the relay subscribed at assembly"
    );
    assert_eq!(
        bus.receiver_count(global_progress_topic()),
        1,
        "the relay subscribed at assembly"
    );
    assert_eq!(
        bus.receiver_count(global_completed_topic()),
        2,
        "the relay and the drain subscribed at assembly"
    );
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    rt.block_on(async {
        client.connect().await.expect("handshake");
        let child = ChildDescriptor::new("bus-probe", "explore", ChildRunMode::Background);
        // Announce, read before the next transition so the latest-status-per-
        // agent sequencer cannot coalesce it away.
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                child: child.clone(),
            },
        );
        read_probe_status(&mut client, "bus-probe", |turn, _, completed, _| {
            turn == 0 && completed.is_none()
        })
        .await;
        // Progress translation through the production emitter.
        let publisher = ChildStatusPublisher::new(Arc::clone(&bus), child);
        publisher.handle(RunLifecycleEvent::TurnCompleted {
            turn: 3,
            cumulative_tokens: 42,
            tool_uses: 1,
            last_activity: Some("probe-activity".into()),
        });
        read_probe_status(&mut client, "bus-probe", |turn, tokens, completed, _| {
            turn == 3 && tokens == 42 && completed.is_none()
        })
        .await;
        // Terminal translation, including the failure shape and summary.
        publisher.handle(RunLifecycleEvent::Completed {
            status: RunCompletionStatus::Failed,
            summary: "probe-summary".into(),
        });
        read_probe_status(&mut client, "bus-probe", |_, _, completed, activity| {
            completed.is_some_and(|c| *c == CompletionStatus::Failed)
                && activity == Some("probe-summary")
        })
        .await;
        // The same completion also reaches the parent's queue through the drain.
        await_probe_notification(&runner, "probe-summary").await;
    });
    drop(client);
    drop(runner);
    let report = rt.block_on(host.begin_shutdown());
    assert_eq!(report.panic_count(), 0, "no task panicked");
    assert_eq!(
        report.outcomes().len(),
        5,
        "serve, relay, drain, refresh, and backfill all tracked"
    );
    assert_eq!(bus.receiver_count(spawned_topic()), 0, "the relay ended");
    assert_eq!(
        bus.receiver_count(global_progress_topic()),
        0,
        "the relay ended"
    );
    assert_eq!(
        bus.receiver_count(global_completed_topic()),
        0,
        "the relay and the drain ended"
    );
    remove_dir_all(&root).ok();
}

/// One switch round's consumption evidence: the round's own bus starts
/// with the relay and drain subscribed, an identifiable background
/// completion published through the production emitter lands in that
/// round's parent queue, and dropping the client lets the round's serve
/// end on its own.
fn consume_switch_round(
    rt: &Runtime,
    runner: &Runner,
    client: Client,
    bus: &Arc<AgentBus>,
    round: usize,
) {
    assert_eq!(
        bus.receiver_count(spawned_topic()),
        1,
        "round {round} relay subscribed"
    );
    assert_eq!(
        bus.receiver_count(global_completed_topic()),
        2,
        "round {round} relay and drain subscribed"
    );
    let marker = format!("summary-{round}");
    let child = ChildDescriptor::new(
        format!("switch-probe-{round}"),
        "explore",
        ChildRunMode::Background,
    );
    let publisher = ChildStatusPublisher::new(Arc::clone(bus), child);
    publisher.handle(RunLifecycleEvent::Completed {
        status: RunCompletionStatus::Completed,
        summary: marker.clone(),
    });
    rt.block_on(await_probe_notification(runner, &marker));
    drop(client);
}

/// Switching on the real assembly: three successive hosts each consume
/// their own probe completion through their own drain, and each host's
/// relay and drain hold their subscriptions from assembly. Stacked
/// releases join every prior host, so after the final release each
/// round's topics have no subscribers left -- a leaked consumer from an
/// earlier round would keep its subscription alive -- and each round's
/// runner is reclaimed.
#[test]
fn test_repeated_switch_consumes() {
    let root = temp_root();
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let mut witnesses: Vec<(Weak<Runner>, Arc<AgentBus>)> = Vec::new();

    let (runner, client, weak, host, lock, bus) = start_host(&root);
    consume_switch_round(&rt, &runner, client, &bus, 0);
    witnesses.push((weak, Arc::clone(&bus)));
    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(
        HostLease::holding(host, lock, "switch-0".into(), Some(runner)),
    )));
    for round in 1..3 {
        let (runner, client, weak, host, lock, bus) = start_host(&root);
        slot.lock().expect("scope").install(
            HostLease::holding(
                host,
                lock,
                format!("switch-{round}"),
                Some(Arc::clone(&runner)),
            ),
            shared_runtime().handle(),
        );
        consume_switch_round(&rt, &runner, client, &bus, round);
        witnesses.push((weak, Arc::clone(&bus)));
        drop(runner);
    }

    release_all(&slot);
    for (round, (weak, bus)) in witnesses.iter().enumerate() {
        assert!(weak.upgrade().is_none(), "round {round} runner reclaimed");
        assert_eq!(
            bus.receiver_count(spawned_topic()),
            0,
            "round {round} relay ended"
        );
        assert_eq!(
            bus.receiver_count(global_completed_topic()),
            0,
            "round {round} relay and drain ended"
        );
    }
    remove_dir_all(&root).ok();
}

/// Counts descriptor writes so a test can tell which store instance the
/// server used. Every method delegates to an in-memory store; only the
/// counter is this test's own.
struct RecordingDescriptorStore {
    inner: InMemoryDescriptorStore,
    writes: AtomicUsize,
}

impl RecordingDescriptorStore {
    fn new() -> Self {
        Self {
            inner: InMemoryDescriptorStore::new(),
            writes: AtomicUsize::new(0),
        }
    }

    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
}

impl SessionDescriptorStore for RecordingDescriptorStore {
    fn read_descriptor(&self, session: SessionId) -> Option<SessionDescriptor> {
        self.inner.read_descriptor(session)
    }

    fn write_descriptor(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<(), SessionDescriptorError> {
        let outcome = self.inner.write_descriptor(session, descriptor);
        if outcome.is_ok() {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        outcome
    }

    fn write_descriptor_if_absent(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<bool, SessionDescriptorError> {
        let written = self.inner.write_descriptor_if_absent(session, descriptor)?;
        if written {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        Ok(written)
    }

    fn update_descriptor(
        &self,
        session: SessionId,
        edit: &mut dyn FnMut(&mut SessionDescriptor),
    ) -> Result<DescriptorUpdate, SessionDescriptorError> {
        let outcome = self.inner.update_descriptor(session, edit)?;
        if matches!(outcome, DescriptorUpdate::Written) {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        Ok(outcome)
    }

    fn delete_descriptor(&self, session: SessionId) {
        self.inner.delete_descriptor(session);
    }
}

/// A rename and a recorded prompt write the same descriptor, so neither can
/// revert the other. The server derives its store from the runner it serves;
/// a second instance over the same root would leave the rename outside the
/// lock the prompt write holds and lose one of the two fields.
#[test]
fn test_rename_preserves_prompt() {
    let root = temp_root();
    let settings_path = root.join("settings.json");
    let descriptors: Arc<RecordingDescriptorStore> = Arc::new(RecordingDescriptorStore::new());
    let mut options = BuildRunnerOptions::default();
    options.descriptor_store = Some(Arc::clone(&descriptors) as Arc<dyn SessionDescriptorStore>);
    options.provider = Some(ProviderResolution::stub());
    let bundle = build_runner(options);
    bundle.runner.set_auto_memory(false);
    bundle.runner.set_auto_dream(false);
    let session = bundle.session;
    let (runner, mut client, _warnings, host) =
        start_local_server(bundle, Some(settings_path), root.clone());
    let wire_session = WireSessionId(session.to_string());
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    rt.block_on(async {
        client.connect().await.expect("handshake completes");
        // The serve loop holds its frame dispatch until the startup trust card
        // is answered, so answer it before sending a request.
        let prompt = timeout(Duration::from_secs(5), client.next_frame())
            .await
            .expect("trust prompt timed out")
            .expect("frame read");
        let ServerFrame::Request(req) = prompt else {
            panic!("the first frame is the trust prompt");
        };
        let ServerRequestPayload::TrustPrompt(_) = req.payload else {
            panic!("the first frame is the trust prompt");
        };
        client
            .send_reverse_response(
                req.req_id,
                ClientResponsePayload::TrustAccept(TrustAccept { accepted: true }),
            )
            .await
            .expect("trust answer sent");
        runner
            .run(session, "fix the login test".to_string())
            .await
            .expect("the turn runs");
        // The prompt write runs detached from the turn; settle here so the
        // rename below is ordered after it rather than racing it.
        runner.settle_prompt_writes().await;
        let rename = RequestId(1);
        client
            .send_request(
                rename,
                FrontendRequest::RenameSession {
                    session_id: wire_session,
                    name: "my session".to_string(),
                },
            )
            .await
            .expect("rename sent");
        loop {
            let frame = timeout(Duration::from_secs(5), client.next_frame())
                .await
                .expect("rename response timed out")
                .expect("frame read");
            match frame {
                ServerFrame::Request(req) => {
                    if matches!(req.payload, ServerRequestPayload::TrustPrompt(_)) {
                        client
                            .send_reverse_response(
                                req.req_id,
                                ClientResponsePayload::TrustAccept(TrustAccept { accepted: true }),
                            )
                            .await
                            .expect("trust answer sent");
                    }
                }
                ServerFrame::Response(env) if env.req_id == rename => {
                    assert!(
                        matches!(env.payload, ResponsePayload::Status(_)),
                        "a rename replies with the applied status"
                    );
                    return;
                }
                _ => {}
            }
        }
    });
    let descriptor = descriptors
        .read_descriptor(session)
        .expect("descriptor exists");
    assert_eq!(descriptor.name.as_deref(), Some("my session"));
    assert_eq!(
        descriptor
            .first_prompt
            .expect("the prompt is recorded")
            .text,
        "fix the login test"
    );
    assert!(
        descriptors.writes() >= 2,
        "prompt and rename both wrote through the runner's store"
    );
    drop(client);
    drop(runner);
    drop(host);
    remove_dir_all(&root).ok();
}
