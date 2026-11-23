//! Decoding the content a client sends in session/prompt into the text the
//! engine runs. The agent declares promptCapabilities false for image, audio,
//! and embedded context at initialize, so a block of any of those kinds is
//! refused with a message naming it rather than silently dropped.

use houyicoder_protocol::frontend::run::ContentBlock;

/// The plain text of a prompt, or an error message naming the first non-text
/// block the agent does not accept.
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
