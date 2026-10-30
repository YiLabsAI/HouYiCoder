//! The per-turn tool-result materialization budget.
//!
//! The design sets B = min(8192, floor(0.10 * W)) tokens where W is the window
//! left after the cache-stable static overhead, the user input, and the reply
//! reserve. This module computes B each turn from the model window and the
//! prior turn's measurement, and pushes it to the installed retention policy so
//! the budget tracks the window rather than a fixed constant. The policy itself
//! (the demote gate) lives in retention.rs; this is only the per-turn sizing.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::ContextBuilder;
use super::SectionKind;
use super::Tokenizer;
use super::retention::{RetentionPolicy, TURN_RESULT_BUDGET_CAP_TOKENS};

impl ContextBuilder {
    /// Install the retention policy the serve path uses for block_ref
    /// ToolResults. The Runner calls this at construction with the cache-
    /// liveness policy sharing its cached-prefix state; the serve path then
    /// holds per-block decisions stable while the cached prefix is live.
    pub(crate) fn set_retention_policy(&self, policy: Arc<dyn RetentionPolicy>) {
        if let Ok(mut g) = self.retention_policy.lock() {
            *g = Some(policy);
        }
    }

    /// Hand out a clone of the tokenizer so a retention policy the builder
    /// installs can tokenize materialized content in the same unit the
    /// assembler measures. The BPE table is a static ref, so the clone is cheap.
    pub(crate) fn tokenizer_clone(&self) -> Tokenizer {
        Tokenizer {
            bpe: self.tokenizer.bpe,
        }
    }

    /// Record the active model's context window so the per-turn tool-result
    /// budget can scale with it. The runner calls this on a model switch.
    pub(crate) fn set_model_window(&self, window: u32) {
        self.model_window.store(window, Ordering::Relaxed);
    }

    /// Compute the per-turn tool-result budget B = min(8192, floor(0.10 * W))
    /// and push it to the installed retention policy. W is the window left
    /// after the cache-stable static overhead (system + tools) measured on the
    /// last turn, the user input, and the reply reserve. Falls back to the cap
    /// when the window or the prior measurement is unknown.
    pub(crate) fn push_turn_budget(&self) {
        let Some(policy) = self
            .retention_policy
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        else {
            return;
        };
        let window = self.model_window.load(Ordering::Relaxed);
        if window == 0 {
            return;
        }
        let static_overhead = self
            .last_measurement
            .lock()
            .ok()
            .and_then(|guard| {
                guard.as_ref().map(|m| {
                    let tools = m.section(SectionKind::Tools).map(|s| s.tokens).unwrap_or(0);
                    m.system_prompt_tokens.saturating_add(tools)
                })
            })
            .unwrap_or(0);
        // Reserve for the reply and the new user input: the output cap plus a
        // quarter of the window for the user turn. The remainder is W.
        let reserve = self
            .model_window_output_cap(window)
            .saturating_add(window / 4);
        let w = window
            .saturating_sub(static_overhead)
            .saturating_sub(reserve);
        let budget = (w / 10).min(TURN_RESULT_BUDGET_CAP_TOKENS);
        policy.set_turn_budget(budget);
    }

    /// The output-token cap for a given window, mirroring the model-window
    /// default so the budget reserve matches the reply space the runner holds.
    fn model_window_output_cap(&self, window: u32) -> u32 {
        (window / 4).clamp(4_096, 32_768)
    }
}
