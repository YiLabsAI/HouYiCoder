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
        Span::styled("CAS storage", bold),
        Span::styled(" \u{00b7} /trajectory", dim),
    ]);
    let results = match cas.results {
        Some(n) => n.to_string(),
        None => "unknown".to_string(),
    };
    let captured = format!(
        "captured: raw {} \u{00b7} {} save failed \u{00b7} {} truncated",
        cas.raw_bytes
            .map(format_byte_size)
            .unwrap_or_else(|| "unknown".to_string()),
        cas.save_failed
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        cas.truncated
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    );
    let mut lines = vec![
        format!(
            "results: {results} \u{00b7} unique blocks: {} \u{00b7} verified: unknown",
            cas.blocks
        ),
        match cas.unmeasured_results.filter(|n| *n > 0) {
            Some(unmeasured) => format!("{captured} \u{00b7} {unmeasured} unmeasured"),
            None => captured,
        },
        assembly_row(cas.assembly.as_ref()),
        index_row(cas),
        reads_row(cas),
    ];
    if !cas.tracked {
        lines.push(
            "this log predates the capture account \u{00b7} only the block count is measured"
                .to_string(),
        );
    }
    let last = lines.len().saturating_sub(1);
    for (i, line) in lines.drain(..).enumerate() {
        rows.push(vec![
            Span::styled(if i == last { "\u{2514} " } else { "\u{251C} " }, dim),
            Span::styled(line, dim),
        ]);
    }
    rows
}

fn reads_row(cas: &CasUsage) -> String {
    if !cas.reads_recorded {
        return "reads: not recorded".to_string();
    }
    let mut row = format!(
        "reads: model {} \u{00b7} UI {} \u{00b7} failed {} \u{00b7} /trajectory",
        cas.model_reads,
        cas.ui_reads
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        cas.failed_reads,
    );
    if cas.unknown_reads > 0 {
        row.push_str(&format!(" \u{00b7} unknown {}", cas.unknown_reads));
    }
    row
}

fn assembly_row(assembly: Option<&CasAssemblyUsage>) -> String {
    let Some(assembly) = assembly else {
        return "current assembly: unknown".to_string();
    };
    let mut forms = vec![
        format!("{} full", assembly.full),
        format!("{} preview", assembly.preview),
        format!("{} pointer", assembly.pointer),
    ];
    if assembly.unavailable > 0 {
        forms.push(format!("{} unavailable", assembly.unavailable));
    }
    if assembly.unclassified > 0 {
        forms.push(format!("{} unclassified", assembly.unclassified));
    }
    let size = assembly_size(assembly).unwrap_or_else(|| "unknown size".to_string());
    format!("current assembly: {} ({})", size, forms.join(", "))
}

fn index_row(cas: &CasUsage) -> String {
    let Some(index) = cas.indexed.as_ref() else {
        return "indexed: unavailable".to_string();
    };
    let coverage = if index.complete {
        "complete"
    } else {
        "partial"
    };
    let as_of = index.as_of.as_deref().unwrap_or("unknown");
    let held = match cas.results {
        Some(results) if results > 0 => format!("{}/{}", index.indexed, results),
        Some(_) => index.indexed.to_string(),
        None => format!("{} of unknown", index.indexed),
    };
    let mut row = format!("indexed: {held} \u{00b7} {coverage} \u{00b7} as of event {as_of}");
    if index.corrupt > 0 {
        row.push_str(&format!(" \u{00b7} {} corrupt", index.corrupt));
    }
    row
}
