//! Free helpers the serve loop and its turn mechanics share, split out so the
//! server module stays under the size gate.

use std::future::pending;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use houyicoder_api::agent_event::ResponseStreamEvent;
use houyicoder_protocol::frontend::run::ContentBlock;

use crate::acp::live::AcpLiveStream;

/// The plain text of a prompt, or an error message naming the first non-text
/// block the agent does not accept. The agent declared promptCapabilities
/// false for image, audio, and embedded context at initialize, so a block of
/// any of those kinds is rejected here rather than silently dropped.
pub(super) fn prompt_text(prompt: Vec<ContentBlock>) -> Result<String, String> {
    let mut text = String::new();
    for block in prompt {
        match block {
            ContentBlock::Text { text: t } => text.push_str(&t),
            ContentBlock::Image { .. } => {
                return Err(
                    "image content is not supported (agent declared promptCapabilities.image false)"
                        .into(),
                );
            }
            ContentBlock::ResourceLink { uri, .. } => {
                return Err(format!(
                    "resource_link attachments are not supported: {uri}"
                ));
            }
            _ => return Err("unsupported content block".into()),
        }
    }
    Ok(text)
}

/// The next live delta, or pending forever when no stream is installed. A
/// select branch that never resolves keeps the run future and the frame
/// reader as the only live branches, so the absence of a stream costs
/// nothing.
pub(super) async fn next_delta(live: &mut Option<AcpLiveStream>) -> Option<ResponseStreamEvent> {
    match live {
        Some(stream) => stream.recv().await,
        None => pending().await,
    }
}

/// The current wall clock as milliseconds since the Unix epoch.
pub(super) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
