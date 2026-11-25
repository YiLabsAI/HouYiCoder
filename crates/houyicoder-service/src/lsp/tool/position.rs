//! The input-coordinate contract of the lsp tool: one-based line and
//! character values a person reads, converted to the protocol's zero-based
//! UTF-16 coordinates, and the identifier under a caret that a workspace
//! symbol query falls back to.

use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::lsp::LspPosition;
use serde_json::Value;

/// One coordinate field, which must be a positive integer that fits the
/// protocol's unsigned line and column.
pub(super) fn positive_coordinate(input: &Value, field: &str) -> Result<u32, ToolError> {
    let value = input
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError::InvalidInput(format!("lsp: {field} (integer) required")))?;
    let value = u32::try_from(value).map_err(|_| {
        ToolError::InvalidInput(format!("lsp: {field} is past the coordinate range"))
    })?;
    if value == 0 {
        return Err(ToolError::InvalidInput(
            "lsp: line and character are one-based".into(),
        ));
    }
    Ok(value)
}

/// The protocol position for one one-based line and character: characters
/// convert to UTF-16 code units so an astral-plane glyph counts twice, the
/// way the protocol counts it. A trailing newline leaves a final empty line
/// that a caret may address.
pub(super) fn utf16_position(
    content: &str,
    line: u32,
    character: u32,
) -> Result<LspPosition, ToolError> {
    if line == 0 || character == 0 {
        return Err(ToolError::InvalidInput(
            "lsp: line and character are one-based".into(),
        ));
    }
    let line_text = content
        .split('\n')
        .nth(line as usize - 1)
        .ok_or_else(|| ToolError::InvalidInput(format!("lsp: line {line} is past the end")))?;
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);
    let char_index = (character - 1) as usize;
    let mut units = 0u32;
    for (seen, ch) in line_text.chars().enumerate() {
        if seen >= char_index {
            break;
        }
        units += ch.len_utf16() as u32;
    }
    if char_index > line_text.chars().count() {
        return Err(ToolError::InvalidInput(format!(
            "lsp: character {character} is past the end of line {line}"
        )));
    }
    Ok(LspPosition {
        line: line - 1,
        character: units,
    })
}

/// The identifier under one one-based position, used when a workspace
/// symbol query names none: the caret's word is almost always what the
/// model meant, and an empty query dumps every symbol in the workspace.
pub(super) fn identifier_at(content: &str, line: u32, character: u32) -> Option<String> {
    let line_text = content.split('\n').nth(line as usize - 1)?;
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);
    let chars: Vec<char> = line_text.chars().collect();
    let index = ((character - 1) as usize).min(chars.len().saturating_sub(1));
    let is_word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '$' | '!' | '\'');
    if chars.is_empty() || !is_word(chars[index]) {
        return None;
    }
    let mut start = index;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = index;
    while end + 1 < chars.len() && is_word(chars[end + 1]) {
        end += 1;
    }
    let word: String = chars[start..=end].iter().collect();
    Some(word.trim_matches('!').to_string())
}
