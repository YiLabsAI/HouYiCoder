//! Canned content for the no-runner path: help/cost text and the /context
//! view payload. Used by Login/Console and the fallback commands so those
//! screens render faithfully before a real runner is connected. Real shared
//! logic (suggestions_for) stays in the parent.

use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::context::stub_breakdown;

use crate::records::{ContextDrillDown, ContextView};

pub fn release_notes() -> String {
    "what's new (canned)\n  - spec strip is now monochrome (one Cyan accent)\n  - per-change evidence diff with approve/reject\n  - inline slash palette (no full-screen popup)\n  - /rewind pops the last stage transition\n  - /resume loads an archived session".into()
}

pub fn help_text() -> String {
    let mut s = String::from("commands (press / to open the palette):\n");
    for cmd in SlashCommand::ALL {
        s.push_str(&format!("  {:<16} {}\n", cmd.name(), cmd.help()));
    }
    s
}

pub fn failing_checks() -> Vec<String> {
    vec![
        "cargo test --workspace: FAIL test_bug_not_reproduced (stub)".to_string(),
        "clippy -D warnings: 2 warnings (stub)".to_string(),
        "fmt --check: clean (placeholder)".to_string(),
    ]
}

/// /context view built from the stub breakdown so the inline block renders
/// the real layout before the analyzer is connected. The drill-down is empty,
/// matching every connected path (the server reply and the cache fast-path
/// both serve a default drill until real drill data lands): a canned view must
/// not fabricate memory files or skills that look like live telemetry.
pub fn context_view() -> ContextView {
    let breakdown = stub_breakdown();
    let suggestions = super::suggestions_for(&breakdown);
    let drill = ContextDrillDown::from_breakdown(&breakdown);
    ContextView {
        breakdown,
        drill,
        suggestions,
    }
}
