//! Canned content for the no-runner path: demo transcript slices, help/cost
//! text, and the /context view payload. Used by Login/Console and the
//! fallback commands so the surfaces render faithfully before a real runner
//! is wired. Real shared logic (suggestions_for) stays in the parent.

use crate::state::TranscriptLine;

pub fn archived_transcript() -> Vec<TranscriptLine> {
    vec![
        TranscriptLine::System("resumed session #sess-archived-042 (canned reload)".to_string()),
        TranscriptLine::User("refactor the spec strip to be monochrome".into()),
        TranscriptLine::Agent("read status.rs and the strip clause line".into()),
        TranscriptLine::Read {
            path: "src/view/status.rs".to_string(),
        },
        TranscriptLine::Edit {
            path: "src/view/status.rs".to_string(),
            summary: "replaced green/yellow/red chips with dim symbols".into(),
        },
        TranscriptLine::System("checkpoint reached (canned)".into()),
    ]
}

pub fn release_notes() -> String {
    "what's new (canned)\n  - spec strip is now monochrome (one Cyan accent)\n  - per-change evidence diff with approve/reject\n  - inline slash palette (no full-screen popup)\n  - /rewind pops the last stage transition\n  - /resume loads an archived session".into()
}

pub fn help_text() -> String {
    let mut s = String::from("commands (press / to open the palette):\n");
    for cmd in houyicoder_protocol::frontend::SlashCommand::ALL {
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
/// the real layout before the analyzer is wired.
pub fn context_view() -> crate::records::ContextView {
    use crate::records::{ContextDrillDown, ContextFileEntry, ContextSkillEntry, ContextView};
    let breakdown = houyicoder_protocol::frontend::context::stub_breakdown();
    let drill = ContextDrillDown {
        memory_files: vec![ContextFileEntry {
            path: "~/.claude/projects/.../MEMORY.md".to_string(),
            tokens: 6_600,
        }],
        skills: vec![
            ContextSkillEntry {
                source: "Built-in".to_string(),
                name: "claude-api".to_string(),
                tokens: 360,
            },
            ContextSkillEntry {
                source: "Built-in".to_string(),
                name: "update-config".to_string(),
                tokens: 240,
            },
            ContextSkillEntry {
                source: "Built-in".to_string(),
                name: "run".to_string(),
                tokens: 800,
            },
        ],
    };
    let suggestions = super::suggestions_for(&breakdown);
    ContextView {
        breakdown,
        drill,
        suggestions,
    }
}
