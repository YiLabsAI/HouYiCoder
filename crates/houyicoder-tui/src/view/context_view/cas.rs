//! The stored-output drill-down in /context: summarizes how many large tool
//! results were offloaded and their token savings in uniform token units.

use houyicoder_protocol::frontend::context::{CasAssemblyUsage, CasSizeUnit, CasUsage};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::command::render::format_tokens;

/// An assembly's carried size in tokens. Byte figures are converted at the
/// standard 4-byte-per-token ratio so the entire /context view uses one unit.
fn assembly_tokens(assembly: &CasAssemblyUsage) -> Option<String> {
    match assembly.unit {
        CasSizeUnit::Tokens => Some(format!("~{} tokens", format_tokens(assembly.size))),
        CasSizeUnit::Bytes => Some(format!("~{} tokens", format_tokens(assembly.size / 4))),
        CasSizeUnit::Unknown => None,
    }
}

/// The stored-output drill-down rows. Empty when the session reported no account.
pub(super) fn cas_rows(cas: Option<&CasUsage>) -> Vec<Vec<Span<'static>>> {
    let Some(cas) = cas else {
        return Vec::new();
    };
    let count = cas.results.filter(|&r| r > 0).unwrap_or(cas.blocks);
    let raw_bytes = cas.raw_bytes.unwrap_or(0);
    if count == 0 && raw_bytes == 0 {
        return Vec::new();
    }
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(vec![
        Span::styled("Saved large outputs", bold),
        Span::styled(" \u{00b7} /trajectory", dim),
    ]);
    let noun = if count == 1 {
        "large result"
    } else {
        "large results"
    };
    let model_str = cas
        .assembly
        .as_ref()
        .and_then(assembly_tokens)
        .unwrap_or_else(|| "~0 tokens".to_string());
    let detail = if raw_bytes > 0 {
        let raw_tokens = format!("~{} tokens", format_tokens(raw_bytes / 4));
        format!("{count} {noun} offloaded ({raw_tokens} raw \u{2192} {model_str} in context)")
    } else {
        format!("{count} {noun} offloaded ({model_str} in context)")
    };
    rows.push(vec![
        Span::styled("\u{2514} ", dim),
        Span::styled(detail, dim),
    ]);
    rows
}
