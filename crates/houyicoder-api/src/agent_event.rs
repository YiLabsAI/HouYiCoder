//! Typed event domains emitted by agent runtime components.

use std::sync::Arc;

use houyicoder_context::MemoryChangeId;

/// Receives events from one typed event domain.
pub trait EventHandler<E>: Send + Sync {
    /// Handle one event.
    fn handle(&self, event: E);
}

impl<E, F> EventHandler<E> for F
where
    F: Fn(E) + Send + Sync,
{
    fn handle(&self, event: E) {
        self(event);
    }
}

/// Incremental model-response output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseStreamEvent {
    /// Incremental assistant response text.
    AssistantTextDelta {
        /// Text appended to the assistant response.
        text: String,
    },
    /// Incremental model reasoning text.
    ReasoningDelta {
        /// Text appended to the reasoning preview.
        text: String,
    },
}

/// Progress emitted while a tool executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecutionEvent {
    /// A progress snapshot for one tool call.
    Progress {
        /// Provider-assigned tool-call identity.
        call_id: String,
        /// Whole seconds elapsed since execution began.
        elapsed_secs: u64,
        /// Running output line count when available.
        output_lines: Option<u64>,
    },
}

/// Typed terminal state for an agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCompletionStatus {
    /// The run produced its final output.
    Completed,
    /// The run transferred control to another agent.
    HandedOff,
    /// The run was interrupted externally.
    Interrupted,
    /// The run failed before producing final output.
    Failed,
    /// Post-run verification rejected the output.
    VerificationFailed,
    /// The run exhausted its configured turn limit.
    TurnLimitReached,
}

/// Coarse lifecycle events for run observers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunLifecycleEvent {
    /// One turn completed and the run will continue.
    TurnCompleted {
        /// One-based turn number.
        turn: u32,
        /// Cumulative token usage through this turn.
        cumulative_tokens: u64,
        /// Number of tool calls in this turn.
        tool_uses: u32,
        /// Most recent tool activity when present.
        last_activity: Option<String>,
    },
    /// The run reached a terminal state.
    Completed {
        /// Typed terminal state.
        status: RunCompletionStatus,
        /// Final or best available response summary.
        summary: String,
    },
}

/// The producer responsible for a set of memory changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryChangeOrigin {
    /// A save_memory call made by the primary agent.
    PrimaryAgent,
    /// Automatic extraction after a run.
    AutoMemory,
    /// Automatic memory consolidation.
    AutoDream,
}

/// The operation applied to one memory key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOperation {
    /// A memory was stored.
    Stored,
    /// A memory was deleted.
    Deleted,
    /// A memory moved to a broader scope.
    Promoted,
    /// A memory moved to a narrower scope.
    Demoted,
}

/// One successful memory operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryChange {
    /// The exact memory key affected.
    pub key: String,
    /// The operation applied to the key.
    pub operation: MemoryOperation,
}

/// Successful memory changes emitted together by one producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryChangedEvent {
    /// Unique notification identity used for delivery deduplication.
    pub id: MemoryChangeId,
    /// Producer responsible for the changes.
    pub origin: MemoryChangeOrigin,
    /// Exact successful operations in append order.
    pub changes: Vec<MemoryChange>,
}

/// A user-visible runtime notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserNoticeEvent {
    /// Text rendered for the user.
    pub message: String,
}

/// Cohesive ownership of optional handlers for every agent event domain.
#[derive(Clone, Default)]
pub struct AgentEventHandlers {
    response_stream: Option<Arc<dyn EventHandler<ResponseStreamEvent>>>,
    tool_execution: Option<Arc<dyn EventHandler<ToolExecutionEvent>>>,
    run_lifecycle: Option<Arc<dyn EventHandler<RunLifecycleEvent>>>,
    memory_changed: Option<Arc<dyn EventHandler<MemoryChangedEvent>>>,
    user_notice: Option<Arc<dyn EventHandler<UserNoticeEvent>>>,
}

impl AgentEventHandlers {
    /// Replace the model-response stream handler.
    pub fn set_response_stream(&mut self, handler: Arc<dyn EventHandler<ResponseStreamEvent>>) {
        self.response_stream = Some(handler);
    }

    /// Return the installed model-response handler.
    pub fn response_stream_handler(&self) -> Option<Arc<dyn EventHandler<ResponseStreamEvent>>> {
        self.response_stream.clone()
    }

    /// Replace the tool-execution handler.
    pub fn set_tool_execution(&mut self, handler: Arc<dyn EventHandler<ToolExecutionEvent>>) {
        self.tool_execution = Some(handler);
    }

    /// Return the installed tool-execution handler.
    pub fn tool_execution_handler(&self) -> Option<Arc<dyn EventHandler<ToolExecutionEvent>>> {
        self.tool_execution.clone()
    }

    /// Replace the run-lifecycle handler.
    pub fn set_run_lifecycle(&mut self, handler: Arc<dyn EventHandler<RunLifecycleEvent>>) {
        self.run_lifecycle = Some(handler);
    }

    /// Replace the memory-change handler.
    pub fn set_memory_changed(&mut self, handler: Arc<dyn EventHandler<MemoryChangedEvent>>) {
        self.memory_changed = Some(handler);
    }

    /// Return the installed memory-change handler.
    pub fn memory_changed_handler(&self) -> Option<Arc<dyn EventHandler<MemoryChangedEvent>>> {
        self.memory_changed.clone()
    }

    /// Replace the user-notice handler.
    pub fn set_user_notice(&mut self, handler: Arc<dyn EventHandler<UserNoticeEvent>>) {
        self.user_notice = Some(handler);
    }

    /// Emit a model-response stream event when a handler is installed.
    pub fn emit_response_stream(&self, event: ResponseStreamEvent) {
        if let Some(handler) = &self.response_stream {
            handler.handle(event);
        }
    }

    /// Emit a tool-execution event when a handler is installed.
    pub fn emit_tool_execution(&self, event: ToolExecutionEvent) {
        if let Some(handler) = &self.tool_execution {
            handler.handle(event);
        }
    }

    /// Emit a run-lifecycle event when a handler is installed.
    pub fn emit_run_lifecycle(&self, event: RunLifecycleEvent) {
        if let Some(handler) = &self.run_lifecycle {
            handler.handle(event);
        }
    }

    /// Emit a memory-change event when it contains successful operations.
    pub fn emit_memory_changed(&self, event: MemoryChangedEvent) {
        if !event.changes.is_empty()
            && let Some(handler) = &self.memory_changed
        {
            handler.handle(event);
        }
    }

    /// Emit a user notice when a handler is installed.
    pub fn emit_user_notice(&self, event: UserNoticeEvent) {
        if let Some(handler) = &self.user_notice {
            handler.handle(event);
        }
    }

    /// Return the installed user-notice handler.
    pub fn user_notice_handler(&self) -> Option<Arc<dyn EventHandler<UserNoticeEvent>>> {
        self.user_notice.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn test_handlers_isolate_event_domains() {
        let responses = Arc::new(Mutex::new(Vec::new()));
        let notices = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = AgentEventHandlers::default();
        let captured = Arc::clone(&responses);
        handlers.set_response_stream(Arc::new(move |event| {
            captured.lock().expect("response lock").push(event);
        }));
        let captured = Arc::clone(&notices);
        handlers.set_user_notice(Arc::new(move |event| {
            captured.lock().expect("notice lock").push(event);
        }));

        handlers.emit_response_stream(ResponseStreamEvent::AssistantTextDelta {
            text: "answer".into(),
        });

        assert_eq!(responses.lock().expect("response lock").len(), 1);
        assert!(notices.lock().expect("notice lock").is_empty());
    }
}
