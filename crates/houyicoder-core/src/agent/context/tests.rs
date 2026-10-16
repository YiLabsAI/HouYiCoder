//! Context assembly and token-accounting tests.
use super::*;
use houyicoder_context::{
    EventId, MemoryEntry, MemorySource, SessionEvent, SessionId, SessionLogEntry,
};
use houyicoder_protocol::llm::ToolDef;
use prompt::SystemPrompt;
use prompt::date::utc_date_line;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::process::id;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[test]
fn test_tokenizer_counts_text() {
    let t = Tokenizer::real();
    let n = t.count("hello world");
    assert!(n > 0, "must count tokens");
    let big = t.count(&"hello world ".repeat(10));
    assert!(big > n, "longer text must count more");
}

#[test]
fn test_tokenizer_no_cjk_undercount() {
    // CJK must not regress to the inaccurate chars-per-token fallback.
    let t = Tokenizer::real();
    let cjk = "\u{4f60}\u{597d}\u{4e16}\u{754c}";
    let n = t.count(cjk);
    assert!(
        n >= cjk.chars().count() as u32 / 2,
        "tiktoken must not undercount CJK like chars/4 would"
    );
}

#[test]
fn test_tokenizer_exact_counts() {
    // Mixed and CJK cases distinguish the intended BPE table from fallback
    // encodings; ASCII cases cover general table drift.
    let t = Tokenizer::real();
    let cases = [
        ("hello world", 2),
        ("fn main() { println!(\"hello\"); }", 9),
        ("\u{4f60}\u{597d}\u{4e16}\u{754c}", 2),
        ("token counts come from tiktoken, not chars/4", 12),
    ];
    for (text, expected) in cases {
        assert_eq!(
            t.count(text),
            expected,
            "BPE drift on {text:?}: expected {expected}"
        );
    }
}

#[test]
fn test_section_label_all_kinds() {
    assert_eq!(SectionKind::SystemPrompt.label(), "System prompt");
    assert_eq!(SectionKind::Tools.label(), "System tools");
    assert_eq!(SectionKind::Memory.label(), "Memory files");
    assert_eq!(SectionKind::Skills.label(), "Skills");
    assert_eq!(SectionKind::Messages.label(), "Messages");
}

#[test]
fn test_measurement_token_count() {
    let m = ContextMeasurement {
        sections: vec![
            Section {
                kind: SectionKind::SystemPrompt,
                tokens: 100,
                items: vec![],
            },
            Section {
                kind: SectionKind::Messages,
                tokens: 50,
                items: vec![],
            },
        ],
        ..Default::default()
    };
    assert_eq!(m.token_count(), 150);
    assert_eq!(m.section(SectionKind::Messages).unwrap().tokens, 50);
    assert!(m.section(SectionKind::Tools).is_none());
}

#[test]
fn test_preview_truncates_long() {
    let short = preview("hi");
    assert_eq!(short, "hi");
    let long = preview(&"x".repeat(100));
    assert!(long.ends_with('\u{2026}'));
    assert_eq!(long.chars().count(), 61);
}

#[test]
fn test_builder_empty_log_system() {
    // A scratch cwd with no memory file: identity-only system prompt.
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-empty-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let b = ContextBuilder::new().with_cwd(scratch);
    let v = b.build(&[]);
    assert!(v.messages.is_empty());
    assert!(!v.system.is_empty(), "system prompt must be non-empty");
    assert!(v.measurement.tool_names.is_empty());
    // Two sections: SystemPrompt + Messages.
    assert_eq!(v.measurement.sections.len(), 2);
    let sys = v
        .measurement
        .section(SectionKind::SystemPrompt)
        .expect("system section");
    assert!(sys.tokens > 0, "system prompt must count > 0 tokens");
    assert!(
        !sys.items.contains(&"Project context".to_string()),
        "no project context row when no memory file"
    );
    assert!(sys.items.contains(&"Identity".to_string()));
    assert_eq!(
        v.measurement.section(SectionKind::Messages).unwrap().tokens,
        0
    );
}

#[test]
fn test_tool_schema_tokens_counted() {
    // R11a: tool schemas occupy the context window. The pre-flight gate
    // must see them or it underestimates by 3-8k/turn on a tool-heavy
    // session — a late trip left the model to overflow mid-response.
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-tools-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let b = ContextBuilder::new().with_cwd(scratch);
    let tool_defs = vec![ToolDef {
        name: "bash".into(),
        description: "run a shell command".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        }),
    }];
    let v = b.build_for_turn(&[], None, None, &tool_defs, None);
    let tools = v
        .measurement
        .section(SectionKind::Tools)
        .expect("Tools section present");
    assert!(
        tools.tokens > 0,
        "tool schema tokens counted: {}",
        tools.tokens
    );
    assert_eq!(tools.items, vec!["bash".to_string()]);
    assert!(
        v.measurement.token_count() >= tools.tokens,
        "token_count includes tool tokens: total={} tools={}",
        v.measurement.token_count(),
        tools.tokens
    );
    assert!(
        b.last_measurement().is_some(),
        "the turn path caches its measurement"
    );
}

#[test]
fn test_clear_drops_measurement() {
    // A cleared session has no assembled context, so the cached figure must go
    // with it. Leaving it would report the size of a context that no longer
    // exists as the current occupancy.
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-clear-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let b = ContextBuilder::new().with_cwd(scratch);
    let v = b.build_for_turn(&[], None, None, &[], None);
    assert!(v.measurement.token_count() > 0, "a turn measures something");
    assert!(b.last_measurement().is_some(), "and caches it");
    b.clear_measurement();
    assert!(
        b.last_measurement().is_none(),
        "clearing drops the cached measurement"
    );
}

#[test]
fn test_bare_build_skips_cache() {
    // Bare assembly must leave the cache alone: a prospective /context on a
    // fresh session must not masquerade as a served turn measurement.
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-nocache-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let b = ContextBuilder::new().with_cwd(scratch);
    let v = b.build(&[]);
    assert!(!v.system.is_empty());
    assert!(
        b.last_measurement().is_none(),
        "bare assembly must not populate last_measurement"
    );
}

#[test]
fn test_builder_injects_memory() {
    // A scratch cwd with an AGENTS.md: project context injected.
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-mem-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    write(scratch.join("AGENTS.md"), "# Scratch Project\n\nrules.").expect("write");
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let v = b.build(&[]);
    assert!(!v.system.is_empty());
    assert!(v.system.contains("Scratch Project"), "memory file injected");
    let sys = v
        .measurement
        .section(SectionKind::SystemPrompt)
        .expect("system section");
    assert!(sys.tokens > 0);
    assert!(sys.items.contains(&"Project context".to_string()));
    remove_dir_all(&scratch).ok();
}

/// The memory behavior guidance (what to save, when, how to treat a recalled
/// memory) is always-on in the system prompt so the agent proactively saves
/// rather than only on an explicit "remember" signal. Constant across turns.
#[test]
fn test_memory_behavior_section_present() {
    let mut dir = temp_dir();
    dir.push(format!("ctx-test-behavior-{}", id()));
    create_dir_all(&dir).expect("mkdir");
    let v = ContextBuilder::new().with_cwd(dir.clone()).build(&[]);
    assert!(v.system.contains("# Memory"), "behavior section heading");
    assert!(
        v.system.contains("## Types of memory"),
        "type taxonomy present"
    );
    assert!(
        v.system.contains("What NOT to save"),
        "what-not-to-save gate present"
    );
    assert!(
        v.system.contains("Before recommending from memory"),
        "trusting-recall guidance present"
    );
    let sys = v
        .measurement
        .section(SectionKind::SystemPrompt)
        .expect("system section");
    assert!(
        sys.items.contains(&"Memory".to_string()),
        "Memory listed in system items"
    );
    remove_dir_all(&dir).ok();
}

/// The render helper produces a system-reminder-wrapped attachment whose
/// body carries the manifest header (type tag in brackets, key, age in
/// parentheses, description hook), the entry body, and a staleness caveat
/// for entries older than a day. Scan format so the model reads the
/// same shape it expects. The wrapper marks the content
/// as injected context, and the assembly merges this text into the user
/// turn — the system prompt itself stays free of recalled memory.
#[test]
fn test_memory_manifest_format_aligned() {
    let entry = MemoryEntry::new(
        "build-gate",
        "make check must stay green",
        MemorySource::Project,
    )
    .with_meta("the build must pass before commit", 0);
    let text = render_recall_text(&[entry]);
    assert!(
        text.contains("<system-reminder>"),
        "system-reminder wrapper present, got: {text}"
    );
    assert!(
        text.contains("# Recalled memories"),
        "Recalled memories heading present"
    );
    assert!(
        text.contains("- [project] build-gate"),
        "manifest header: type tag + key, got: {text}"
    );
    assert!(
        text.contains("the build must pass before commit"),
        "description hook in header"
    );
    assert!(
        text.contains("make check must stay green"),
        "body content inlined after the header"
    );
    assert!(
        text.contains("point-in-time"),
        "staleness caveat for an epoch-old entry, got: {text}"
    );
    assert!(
        text.ends_with("</system-reminder>"),
        "wrapper closes, got: {text}"
    );
}

/// A fresh entry renders without a staleness caveat.
#[test]
fn test_memory_manifest_no_caveat() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = MemoryEntry::new("fresh-note", "just saved a fact", MemorySource::User)
        .with_meta("a recent fact", now);
    let text = render_recall_text(&[entry]);
    assert!(
        text.contains("- [user] fresh-note (today)"),
        "fresh entry shows today, got: {text}"
    );
    assert!(
        !text.contains("point-in-time"),
        "no staleness caveat on a fresh entry, got: {text}"
    );
}

/// The context breakdown attributes recalled-memory tokens separately without
/// changing the cache-stable system prompt or double-counting message input.
#[test]
fn test_context_partitions_recall_tokens() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-memsec-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let mem_text = render_recall_text(&[MemoryEntry::new(
        "build-gate",
        "make check must stay green",
        MemorySource::Project,
    )
    .with_meta("the build must pass", 0)]);
    let events = vec![
        SessionLogEntry {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "what is the build rule".into(),
            },
        },
        SessionLogEntry {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            event: SessionEvent::MemoryRecall {
                text: mem_text.clone(),
                keys: vec!["build-gate".to_string()],
                bytes: mem_text.len() as u32,
            },
        },
    ];
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let v = b.build(&events);
    let mem = v
        .measurement
        .section(SectionKind::Memory)
        .expect("Memory section present when a memory-recall event is in the log");
    assert!(
        mem.tokens > 0,
        "Memory section must count the attachment tokens"
    );
    assert!(
        mem.items.contains(&"build-gate".to_string()),
        "Memory section items are the surfaced keys"
    );
    assert!(
        !v.system.contains("# Recalled memories"),
        "system prompt stays byte-frozen, no recalled memory in it"
    );
    // No double-count: the assembly merges the memory text into the user
    // message, so msg_tokens already includes it. Messages (non-memory
    // input) + Memory (attachment) must equal the full input, and the total
    // must equal system + full input — not system + input + memory.
    let tok = Tokenizer::new();
    // The assembly merges the memory text into the user message with a
    // newline joiner; count the merged content as one string (BPE is not
    // additive across pieces).
    let merged = format!("what is the build rule\n{mem_text}");
    let full_input = tok.count(&merged);
    let messages_tokens = v.measurement.section(SectionKind::Messages).unwrap().tokens;
    let sys_tokens = v
        .measurement
        .section(SectionKind::SystemPrompt)
        .unwrap()
        .tokens;
    assert_eq!(
        messages_tokens + mem.tokens,
        full_input,
        "memory must not be double-counted (Messages + Memory == full input)"
    );
    assert_eq!(
        v.measurement.token_count(),
        sys_tokens + full_input,
        "total == system + full input, no memory inflation"
    );
    remove_dir_all(&scratch).ok();
}

#[test]
fn test_skill_section_recomputed() {
    // A SkillListing event in the log: the context assembly merges its text
    // into the user message, and the Skills section attributes the attachment
    // tokens so /context shows the discovery cost without double-counting.
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-skillsec-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let listing_text = "- commit: commit changes - after edits\n- review: review a PR";
    let events = vec![
        SessionLogEntry {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "help me commit".into(),
            },
        },
        SessionLogEntry {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            event: SessionEvent::SkillListing {
                text: listing_text.to_string(),
                bytes: listing_text.len() as u32,
                content_hash: 0,
            },
        },
    ];
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let v = b.build(&events);
    let skill = v
        .measurement
        .section(SectionKind::Skills)
        .expect("Skills section present when a listing event is in the log");
    assert!(
        skill.tokens > 0,
        "Skills section must count the listing tokens"
    );
    // No double-count: the assembly merges the listing text into the user
    // message, so Messages (non-listing input) + Skills (attachment) must
    // equal the full merged input.
    let tok = Tokenizer::new();
    let merged = format!("help me commit\n{listing_text}");
    let full_input = tok.count(&merged);
    let messages_tokens = v.measurement.section(SectionKind::Messages).unwrap().tokens;
    assert_eq!(
        messages_tokens + skill.tokens,
        full_input,
        "listing must not be double-counted (Messages + Skills == full input)"
    );
    remove_dir_all(&scratch).ok();
}

#[test]
fn test_build_grid_200k_100() {
    let cats = vec![
        CategoryBreakdown {
            label: "System prompt".into(),
            color_hint: 0,
            tokens: 1_800,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Messages".into(),
            color_hint: 0,
            tokens: 120_000,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Free space".into(),
            color_hint: 0,
            tokens: 78_200,
            is_deferred: false,
            is_reserved: false,
        },
    ];
    let grid = build_grid(&cats, 200_000, 100);
    // 10x10 = 100 cells, 10 rows of 10.
    assert_eq!(grid.len(), 10, "10 rows");
    assert!(grid.iter().all(|r| r.len() <= 10), "rows <= 10 wide");
    let total: usize = grid.iter().map(|r| r.len()).sum();
    assert_eq!(total, 100, "grid fills exactly 100 cells");
}

#[test]
fn test_measurement_breakdown_real() {
    // The real /context breakdown derives from the measurement sections:
    // one category per section kind, a trailing Free-space row, a grid,
    // and the real per-section token counts (no chars/4 estimate).
    let measurement = ContextMeasurement {
        tool_names: Vec::new(),
        sections: vec![
            Section {
                kind: SectionKind::SystemPrompt,
                tokens: 1_800,
                items: Vec::new(),
            },
            Section {
                kind: SectionKind::Messages,
                tokens: 30_000,
                items: Vec::new(),
            },
        ],
    };
    let bd = measurement.breakdown("test", 200_000);
    assert_eq!(bd.total_tokens, 31_800);
    assert_eq!(bd.context_window, 200_000);
    // Two section categories + one Free-space row.
    assert_eq!(bd.categories.len(), 3);
    assert_eq!(bd.categories[0].label, "System prompt");
    assert_eq!(bd.categories[0].tokens, 1_800);
    assert_eq!(bd.categories[1].label, "Messages");
    assert_eq!(bd.categories[2].label, "Free space");
    assert_eq!(bd.categories[2].tokens, 200_000 - 31_800);
    assert!(!bd.grid.is_empty(), "breakdown builds a proportional grid");
    assert!(bd.cache_breakpoint.is_none());
}

#[test]
fn test_build_empty_events_view() {
    // build(&[]) with no events is the fresh-session /context path
    // (last_measurement() None → prospective_measurement() → build(&[])).
    let cb = ContextBuilder::new();
    let view = cb.build(&[]);
    assert!(
        view.measurement.token_count() > 0,
        "prospective view must have system prompt + tools tokens, not 0"
    );
    let messages = view
        .measurement
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Messages)
        .expect("messages section present in the prospective view");
    assert_eq!(
        messages.tokens, 0,
        "messages tokens must be 0 (no turn run)"
    );
    let bd = view.measurement.breakdown("test", 200_000);
    assert!(
        bd.categories.len() >= 3,
        "prospective breakdown has system prompt + tools + messages + free space"
    );
    assert!(!bd.grid.is_empty(), "prospective breakdown builds a grid");
}

/// The system prompt remains byte-stable as the event log grows in one cwd.
#[test]
fn test_system_block_stable_turns() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-stable-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    write(scratch.join("AGENTS.md"), "# Stable Project\n\nrules.").expect("write");
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let s = SessionId::new();
    let turn1 = vec![SessionLogEntry {
        id: EventId::new(),
        session: s,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "first".into(),
        },
    }];
    let turn2 = vec![
        SessionLogEntry {
            id: EventId::new(),
            session: s,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "first".into(),
            },
        },
        SessionLogEntry {
            id: EventId::new(),
            session: s,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::AssistantMessage {
                text: "reply".into(),
                thinking: None,
            },
        },
        SessionLogEntry {
            id: EventId::new(),
            session: s,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "second".into(),
            },
        },
    ];
    let v1 = b.build(&turn1);
    let v2 = b.build(&turn2);
    assert_eq!(
        v1.system, v2.system,
        "system prompt byte-stable across turns (event log grew but system did not)"
    );
    let sys1 = v1
        .measurement
        .section(SectionKind::SystemPrompt)
        .expect("system section");
    let sys2 = v2
        .measurement
        .section(SectionKind::SystemPrompt)
        .expect("system section");
    assert_eq!(sys1.tokens, sys2.tokens, "system section tokens stable");
    remove_dir_all(&scratch).ok();
}

/// Recalled memory remains in the message stream and outside the static
/// system prompt.
#[test]
fn test_recall_not_system_section() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-recall-sep-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let s = SessionId::new();
    let recall_text = "DEPLOY_COMMAND=make deploy".to_string();
    let events = vec![
        SessionLogEntry {
            id: EventId::new(),
            session: s,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "how to deploy".into(),
            },
        },
        SessionLogEntry {
            id: EventId::new(),
            session: s,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::MemoryRecall {
                text: recall_text.clone(),
                keys: vec!["deploy".into()],
                bytes: recall_text.len() as u32,
            },
        },
    ];
    let v = b.build(&events);
    // The system prompt does NOT carry the recalled text — the cached prefix
    // stays byte-stable across turns with different recall.
    assert!(
        !v.system.contains(&recall_text),
        "recalled memory must not enter the system prompt static section"
    );
    let sys = v
        .measurement
        .section(SectionKind::SystemPrompt)
        .expect("system section");
    assert!(
        !sys.items.iter().any(|i| i.contains("DEPLOY_COMMAND")),
        "system items must not list recall content"
    );
    // The recall text is in the user message (merged by the assembly) —
    // the model sees it, just not in the static prefix.
    let msgs = v
        .measurement
        .section(SectionKind::Messages)
        .expect("messages section");
    assert!(
        msgs.tokens > 0,
        "messages section carries the merged recall"
    );
    remove_dir_all(&scratch).ok();
}

/// The dynamic boundary keeps the static prefix stable across working
/// directories while project context remains in the session-scoped suffix.
#[test]
fn test_boundary_splits_static_dynamic() {
    let mut dir_a = temp_dir();
    dir_a.push(format!("ctx-test-boundary-a-{}", id()));
    create_dir_all(&dir_a).expect("mkdir a");
    write(dir_a.join("AGENTS.md"), "# Project A\n\nrules A.").expect("write a");
    let mut dir_b = temp_dir();
    dir_b.push(format!("ctx-test-boundary-b-{}", id()));
    create_dir_all(&dir_b).expect("mkdir b");
    write(dir_b.join("AGENTS.md"), "# Project B\n\nrules B.").expect("write b");

    let pa = SystemPrompt::build(&dir_a);
    let pb = SystemPrompt::build(&dir_b);

    // The static prefix (before dynamic_boundary) is byte-identical across
    // different cwds + different project memory files.
    let prefix_a = &pa.text[..pa.dynamic_boundary];
    let prefix_b = &pb.text[..pb.dynamic_boundary];
    assert_eq!(
        prefix_a, prefix_b,
        "static prefix byte-stable across cwds (only the dynamic suffix differs)"
    );
    assert!(
        !prefix_a.contains("Project A") && !prefix_a.contains("Project B"),
        "project context lives in the dynamic suffix, not the static prefix"
    );
    // The dynamic suffix differs across cwds (project A vs B).
    let suffix_a = &pa.text[pa.dynamic_boundary..];
    let suffix_b = &pb.text[pb.dynamic_boundary..];
    assert!(suffix_a.contains("Project A"), "suffix A carries project A");
    assert!(suffix_b.contains("Project B"), "suffix B carries project B");
    assert_ne!(suffix_a, suffix_b, "dynamic suffix differs per cwd");

    remove_dir_all(&dir_a).ok();
    remove_dir_all(&dir_b).ok();
}

/// The dynamic boundary remains present when no project memory exists.
#[test]
fn test_boundary_set_without_project() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-boundary-empty-{}", id()));
    create_dir_all(&scratch).expect("mkdir");
    let p = SystemPrompt::build(&scratch);
    assert!(
        p.dynamic_boundary > 0,
        "boundary is set even without project context"
    );
    // No project context exists here, so the date is the whole suffix.
    let suffix = &p.text[p.dynamic_boundary..];
    assert_eq!(
        suffix,
        format!("\n\n{}", utc_date_line()),
        "the dynamic suffix carries the run's date"
    );
    remove_dir_all(&scratch).ok();
}

/// The date grounds the run without entering a message body. The system prompt
/// carries it past the boundary, so it neither rotates the cacheable prefix nor
/// becomes text the model can quote but the session log cannot verify.
#[test]
fn test_date_grounds_system_prompt() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-date-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let events = vec![SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "first ask".into(),
        },
    }];
    let v = ContextBuilder::new()
        .with_cwd(scratch.clone())
        .build(&events);
    let date = utc_date_line();
    assert!(
        v.system.ends_with(&date),
        "the system prompt grounds the run's date: {}",
        v.system
    );
    let prefix = &v.system[..v.dynamic_boundary];
    assert!(
        !prefix.contains(&date),
        "the daily date stays out of the cacheable prefix"
    );
    let sent = serde_json::to_string(&v.messages).expect("messages serialize");
    assert!(
        !sent.contains(&date),
        "no message body carries the injected date: {sent}"
    );
    remove_dir_all(&scratch).ok();
}

/// The assemble path carries the boundary into AssembledContext, so the
/// request builder threads a real interior offset instead of dropping it. A
/// zero would collapse every prompt-cache key onto one value; the whole
/// length would let the cwd suffix rotate it. The memory index lands in the
/// static prefix, so the cacheable segment survives a working-directory
/// change and only the project-context suffix diverges.
#[test]
fn test_assemble_threads_boundary() {
    let index = "- [Note](note.md) stable summary";
    let mut dir_a = temp_dir();
    dir_a.push(format!("ctx-test-assemble-a-{}", id()));
    create_dir_all(&dir_a).expect("mkdir a");
    write(dir_a.join("AGENTS.md"), "# Project A\n\nrules A.").expect("write a");
    let mut dir_b = temp_dir();
    dir_b.push(format!("ctx-test-assemble-b-{}", id()));
    create_dir_all(&dir_b).expect("mkdir b");
    write(dir_b.join("AGENTS.md"), "# Project B\n\nrules B.").expect("write b");

    let a = ContextBuilder::new()
        .with_cwd(dir_a.clone())
        .build_prospective(&[], Some(index));
    let b = ContextBuilder::new()
        .with_cwd(dir_b.clone())
        .build_prospective(&[], Some(index));

    assert!(a.dynamic_boundary > 0, "boundary is interior, not zero");
    assert!(
        a.dynamic_boundary < a.system.len(),
        "boundary precedes the dynamic suffix"
    );
    let prefix_a = &a.system[..a.dynamic_boundary];
    let prefix_b = &b.system[..b.dynamic_boundary];
    assert_eq!(
        prefix_a, prefix_b,
        "static prefix byte-stable across cwds through the assemble path"
    );
    assert!(
        prefix_a.contains("stable summary"),
        "memory index sits in the static prefix, not the suffix"
    );
    assert!(
        a.system[a.dynamic_boundary..].contains("Project A"),
        "suffix carries the per-cwd project context"
    );
    assert_ne!(
        a.system[a.dynamic_boundary..],
        b.system[b.dynamic_boundary..],
        "dynamic suffix differs per cwd"
    );

    remove_dir_all(&dir_a).ok();
    remove_dir_all(&dir_b).ok();
}

#[test]
fn test_agent_directory_round_trip() {
    let cb = ContextBuilder::new();
    assert!(cb.agent_directory().is_none(), "unset directory is None");
    cb.set_agent_directory("## Available agents\n\n- explore: fast".into());
    assert_eq!(
        cb.agent_directory().as_deref(),
        Some("## Available agents\n\n- explore: fast"),
    );
}

/// The prospective path must measure the REAL tool definitions and memory
/// index, not an empty list: a /context before the first turn has to report
/// the same tools section the first turn will send, or the pane shows zero
/// and the next turn contradicts it.
#[test]
fn test_prospective_measures_real_tools() {
    let cb = ContextBuilder::new();
    let tool_defs = vec![ToolDef {
        name: "bash".to_string(),
        description: "Run a shell command in the workspace.".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    let with_tools = cb.build_prospective(&tool_defs, None);
    let tools = with_tools
        .measurement
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Tools)
        .expect("tools section present when definitions are supplied");
    assert!(
        tools.tokens > 0,
        "the tools section must measure the real definitions"
    );
    assert_eq!(tools.items, vec!["bash".to_string()]);

    let without = cb.build_prospective(&[], None);
    assert!(
        without
            .measurement
            .sections
            .iter()
            .all(|s| s.kind != SectionKind::Tools),
        "no tools section when no definitions are supplied"
    );
    assert!(
        cb.last_measurement().is_none(),
        "the prospective build must not populate the turn cache"
    );
}
