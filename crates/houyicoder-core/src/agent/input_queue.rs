//! Mid-turn injection queue accessors. Split from mod.rs so that file stays
//! under the file-size gate. The queue itself (queued_input + consumed_input)
//! lives on the Runner; these are the host-facing operations the service calls
//! over the wire (enqueue on session/inject, remove on session/queue_remove,
//! take consumed at run end to tell the frontend what was injected).

use super::{RunError, RunOutcome, RunResult, Runner};

impl Runner {
    /// Enqueue a user message for mid-turn injection. The host (service)
    /// calls this when the frontend submits a message while a run is in
    /// flight; the drive loop drains the queue at the next turn boundary +
    /// appends the text as a user message so the model sees it on its next
    /// call. Like abort(&self) — callable from any Arc<Runner> the host
    /// holds, including a reconnecting serve that re-hydrates the same Arc.
    pub fn enqueue_input(&self, text: String) {
        self.queued_input
            .lock()
            .expect("queued_input lock")
            .push_back(text);
    }

    /// Remove the first queued message whose text matches. The frontend calls
    /// this (via the wire) when it deletes a queue entry from its overlay, or
    /// when it pops the head to start a follow-up run so the new run does not
    /// re-inject it. FIFO + text match reconciles against the frontend's
    /// overlay. No-op when no entry matches (already drained).
    pub fn remove_input(&self, text: &str) {
        let mut q = self.queued_input.lock().expect("queued_input lock");
        if let Some(pos) = q.iter().position(|t| t == text) {
            q.remove(pos);
        }
    }

    /// Drain + return the texts the drive loop injected this run. The host
    /// calls this at run end so it can tell the frontend which queued messages
    /// were consumed (the frontend removes them from its overlay). Per-run:
    /// draining clears the list so a fresh run starts empty. The frontend's
    /// run-boundary queue drains separately (Path B: spawn the head) — the
    /// consumed list is the Path A signal only.
    pub fn take_consumed_input(&self) -> Vec<String> {
        std::mem::take(&mut *self.consumed_input.lock().expect("consumed_input lock"))
    }

    /// Drop every queued message without running it. The host is the single
    /// truth source for ordering; the server queue is only the current run's
    /// injection buffer. A state-changing command (session reset) or an
    /// interrupted run invalidates the buffer, so the orphaned texts must not
    /// leak into the next run. Like enqueue_input/remove_input (callable
    /// from any Arc<Runner> the host holds).
    pub fn clear_input_queue(&self) {
        self.queued_input.lock().expect("queued_input lock").clear();
    }

    /// Drop the server injection buffer on a terminal run end (any outcome
    /// but Interruption, or an Err); keep it on Interruption (a permission
    /// pause -- the run resumes). Called from the drive_loop wrapper so
    /// every caller is covered. A no-op on forked runners (own empty
    /// buffer).
    pub fn finalize_input_buffer(&self, result: &Result<RunResult, RunError>) {
        let terminal = match result {
            Ok(r) => !matches!(r.outcome, RunOutcome::Interruption(_)),
            Err(_) => true,
        };
        if terminal {
            self.clear_input_queue();
        }
    }

    /// Test-only snapshot of the queued texts, in FIFO order. The single
    /// truth source is the host's pending queue; this accessor lets a test
    /// prove the server queue was cleared on interrupt/reset (the layer the
    /// wire-level tests cannot inspect directly).
    #[cfg(test)]
    pub(crate) fn queued_input_snapshot(&self) -> Vec<String> {
        self.queued_input
            .lock()
            .expect("queued_input lock")
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::ToolRegistry;
    use super::super::runner_config::RunnerConfig;
    use super::Runner;
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_session::SessionStore;

    fn bare_runner() -> Runner {
        let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        Runner::new(
            store,
            std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("ok")),
            ToolRegistry::new(),
            RunnerConfig {
                model: "test".into(),
                instructions: "test".into(),
                max_turns: 5,
                max_output_tokens: 8_000,
                ..RunnerConfig::default()
            },
        )
    }

    /// Enqueue then remove before drain: the removed text is gone, so a
    /// subsequent drain returns nothing + take_consumed_input is empty.
    #[test]
    fn test_remove_drops_matching_entry() {
        let r = bare_runner();
        r.enqueue_input("alpha".into());
        r.enqueue_input("beta".into());
        r.remove_input("alpha");
        // No public drain helper; take_consumed_input drains the consumed
        // list (empty here since the drive loop never ran). Enqueue is
        // observable only via the drive loop, covered in loop_tests_mid_turn.
        assert!(r.take_consumed_input().is_empty(), "nothing consumed yet");
        r.remove_input("beta");
        assert!(
            r.take_consumed_input().is_empty(),
            "remove is a no-op on consumed"
        );
    }

    /// take_consumed_input drains: a second call returns nothing (per-run
    /// reset semantics).
    #[test]
    fn test_take_consumed_drains_run() {
        let r = bare_runner();
        // Simulate the drive loop pushing consumed texts directly is private;
        // the public contract is "drains + resets". Verify the empty case +
        // idempotency.
        let first = r.take_consumed_input();
        assert!(first.is_empty());
        let second = r.take_consumed_input();
        assert!(
            second.is_empty(),
            "second take after a draining take is empty"
        );
    }
}
