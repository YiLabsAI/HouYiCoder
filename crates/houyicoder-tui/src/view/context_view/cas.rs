//! The block-store account in the /context legend and drill-down: what the
//! capture kept, what the newest request carried, what the index covers, and
//! who read a stored result.

use houyicoder_protocol::frontend::context::{CasAssemblyUsage, CasSizeUnit, CasUsage};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::command::render::format_tokens;
use crate::view::byte_size::format_byte_size;

/// The legend's one block-store line, or None when the session reported no
/// stored-block account at all. A figure the log does not carry reads as
/// unknown; a measured zero stays a zero.
pub(super) fn cas_legend(cas: Option<&CasUsage>) -> Option<String> {
    let cas = cas?;
    if !cas.tracked {
        return Some(if cas.blocks > 0 {
            format!("cas: unknown \u{00b7} {} blocks", cas.blocks)
        } else {
            "cas: unknown".to_string()
        });
    }
    let mut parts = vec![match cas.results {
        Some(results) => format!("{results} results"),
        None => "unknown results".to_string(),
    }];
    parts.push(match cas.raw_bytes {
        Some(bytes) => format!("raw {}", format_byte_size(bytes)),
        None => "raw unknown".to_string(),
    });
    parts.push(match cas.assembly.as_ref().and_then(assembly_size) {
        Some(size) => format!("model {size}"),
        None => "model unknown".to_string(),
    });
    match cas.save_failed {
        Some(failed) if failed > 0 => parts.push(format!("{failed} save failed")),
        None => parts.push("save failures unknown".to_string()),
        Some(_) => {}
    }
    match cas.truncated {
        Some(truncated) if truncated > 0 => parts.push(format!("{truncated} truncated")),
        None => parts.push("truncations unknown".to_string()),
        Some(_) => {}
    }
    Some(format!("cas: {}", parts.join(" \u{00b7} ")))
}

/// An assembly's carried size in the unit its record named, or None when the
/// unit is one this build does not know: a byte count printed as tokens would
/// misstate the cost.
fn assembly_size(assembly: &CasAssemblyUsage) -> Option<String> {
    match assembly.unit {
        CasSizeUnit::Tokens => Some(format!("~{} tokens", format_tokens(assembly.size))),
        CasSizeUnit::Bytes => Some(format_byte_size(assembly.size)),
        CasSizeUnit::Unknown => None,
    }
}

/// The block-store drill-down rows. Empty when the session reported no account.
pub(super) fn cas_rows(cas: Option<&CasUsage>) -> Vec<Vec<Span<'static>>> {
    let Some(cas) = cas else {
        return Vec::new();
    };
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(vec![
        Span::styled("Saved large outputs", bold),
        Span::styled(" \u{00b7} /trajectory", dim),
    ]);
    let results = cas.results.unwrap_or(0);
    let raw_str = cas
        .raw_bytes
        .map(format_byte_size)
        .unwrap_or_else(|| "0 B".to_string());
    let model_str = cas
        .assembly
        .as_ref()
        .and_then(assembly_size)
        .unwrap_or_else(|| "~0 tokens".to_string());
    rows.push(vec![
        Span::styled("\u{2514} ", dim),
        Span::styled(
            format!(
                "{results} large results offloaded ({raw_str} raw \u{2192} {model_str} in context)"
            ),
            dim,
        ),
    ]);
    rows
}
