//! Stream event folding, truncation classification, and length-recovery nudge
//! emission for a single model call.

use houyicoder_api::agent_event::{EventHandler, ResponseStreamEvent};
use houyicoder_context::{SessionEvent, SessionId, TruncationSignal};
use houyicoder_protocol::llm::{LlmEvent, OutputItem, ProviderError};

use super::stream_fold::StreamFold;
use crate::agent::Tokenizer;
use crate::agent::truncation::{classify_truncation_signal, is_length_reason};
use crate::agent::{RunError, Runner, new_event};

/// Normalize the finish reason and classify silent truncation on a completed
/// stream fold, returning the raw provider finish reason, the self-counted
/// output tokens, and the truncation signal.
pub(super) fn classify_stream_truncation(
    state: &mut StreamFold,
    max_output_tokens: u32,
) -> (Option<String>, u32, TruncationSignal) {
    let raw_finish_reason = state.finish_reason.clone();
    if let Some(reason) = &state.finish_reason
        && reason != "length"
        && is_length_reason(reason)
    {
        state.finish_reason = Some("length".into());
    }
    let self_count_output_tokens = Tokenizer::new().count(&state.assistant_text);
    let mut truncation_signal = TruncationSignal::None;
    if state.finish_reason.as_deref() != Some("length") {
        truncation_signal = classify_truncation_signal(
            &state.assistant_text,
            &state.usage,
            self_count_output_tokens,
            max_output_tokens,
        );
        if truncation_signal != TruncationSignal::None {
            state.finish_reason = Some("length".into());
        }
    }
    (
        raw_finish_reason,
        self_count_output_tokens,
        truncation_signal,
    )
}

/// Append a user-visible truncation notice when length recovery is exhausted.
pub(super) fn note_exhausted_truncation(state: &mut StreamFold) {
    if state.finish_reason.as_deref() != Some("length") {
        return;
    }
    if state.assistant_text.is_empty() {
        state.assistant_text = if state.reasoning.is_empty() {
            "*(output truncated at the token cap — no reply produced after recovery)*".to_string()
        } else {
            "*(the model reasoned but produced no answer before the token cap — recovery exhausted)*"
                .to_string()
        };
    } else {
        state
            .assistant_text
            .push_str("\n\n*…output truncated at the token cap — recovery exhausted.*");
    }
}

impl Runner {
    /// Append the resume-direct nudge as a persisted MetaUser event so the
    /// next serve picks it up and the model continues the cut reply.
    pub(super) async fn append_resume_nudge(&self, session: SessionId) -> Result<(), RunError> {
        const NUDGE: &str = "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.";
        self.store
            .append(new_event(
                session,
                SessionEvent::MetaUser {
                    text: NUDGE.to_string(),
                },
            ))
            .await?;
        Ok(())
    }

    /// Fold one streamed LlmEvent into turn state and emit response events.
    /// A ProviderError event is terminal for the turn.
    pub(super) async fn fold_event(
        &self,
        ev: LlmEvent,
        state: &mut StreamFold,
        session: SessionId,
        response_handler: Option<&dyn EventHandler<ResponseStreamEvent>>,
    ) -> Result<(), RunError> {
        match ev {
            LlmEvent::TextDelta { text, .. } => {
                if let Some(handler) = response_handler {
                    handler.handle(ResponseStreamEvent::AssistantTextDelta { text: text.clone() });
                }
                self.store
                    .append(new_event(
                        session,
                        SessionEvent::AssistantTextDelta { text: text.clone() },
                    ))
                    .await?;
                state.assistant_text.push_str(&text);
            }
            LlmEvent::ReasoningDelta { text, .. } => {
                if let Some(handler) = response_handler {
                    handler.handle(ResponseStreamEvent::ReasoningDelta { text: text.clone() });
                }
                state.reasoning.push(text);
            }
            LlmEvent::ToolCall { id, name, input } => {
                state
                    .tool_calls
                    .push(OutputItem::ToolCall { id, name, input });
            }
            LlmEvent::Finish { reason, usage: u } => {
                if let Some(u) = u {
                    state.usage = u;
                }
                state.finish_reason = Some(reason);
            }
            LlmEvent::ProviderError { .. } => {
                return Err(RunError::ProviderFatal(ProviderError::Unknown(
                    "provider error event mid-stream".into(),
                )));
            }
            LlmEvent::TextStart { .. }
            | LlmEvent::TextEnd { .. }
            | LlmEvent::ReasoningStart { .. }
            | LlmEvent::ReasoningEnd { .. }
            | LlmEvent::ToolInputStart { .. }
            | LlmEvent::ToolInputDelta { .. }
            | LlmEvent::ToolInputEnd { .. }
            | LlmEvent::ToolResult { .. }
            | LlmEvent::ToolError { .. }
            | LlmEvent::StepStart { .. }
            | LlmEvent::StepFinish { .. } => {}
        }
        Ok(())
    }
}
