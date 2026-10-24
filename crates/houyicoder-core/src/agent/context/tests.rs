//! Context assembly and token-accounting tests.
use super::*;
use crate::agent::multi_agent::registry::built_in_general_purpose;
use crate::agent::skill::listing::format_skill_listing;
use houyicoder_api::skill::{SkillDescriptor, SkillUsage};
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
use std::slice::from_ref;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Assembly inputs carrying tools and a memory index but no skills. Call sites
/// that exercise the skill rows build the struct directly.
fn assembly_input<'a>(
    tool_defs: &'a [ToolDef],
    memory_index: Option<&'a str>,
) -> AssemblyInput<'a> {
    AssemblyInput {
        tool_defs,
        memory_index,
        announced_skills: &[],
        discovered_skills: &[],
    }
}

/// A skill descriptor with every optional field empty, for the skill-row cases
/// that only exercise the listing entry.
fn sample_descriptor() -> SkillDescriptor {
    SkillDescriptor {
        name: "sample".to_string(),
        description: String::new(),
        when_to_use: None,
        argument_hint: None,
        disable_model_invocation: false,
        user_invocable: true,
        body_token_estimate: 0,
        allowed_tools: Vec::new(),
        allowed_mach_services: Vec::new(),
        allow_app_launch: false,
    }
}

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
            },
            Section {
                kind: SectionKind::Messages,
                tokens: 50,
            },
        ],
        ..Default::default()
    };
    assert_eq!(m.token_count(), 150);
    assert_eq!(m.section(SectionKind::Messages).unwrap().tokens, 50);
    assert!(m.section(SectionKind::Tools).is_none());
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
    assert_eq!(
        v.measurement.section(SectionKind::Tools).unwrap().tokens,
        0,
        "no declared tool, no tool cost"
    );
    // Every section is present in a fixed order, so the legend reports the
    // same categories on every session instead of reshaping per turn.
    let kinds: Vec<SectionKind> = v.measurement.sections.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        vec![
            SectionKind::SystemPrompt,
            SectionKind::Tools,
            SectionKind::Memory,
            SectionKind::Skills,
            SectionKind::Messages,
        ]
    );
    let sys = v
        .measurement
        .section(SectionKind::SystemPrompt)
        .expect("system section");
    assert!(sys.tokens > 0, "system prompt must count > 0 tokens");
    assert!(
        v.measurement.memory_files.is_empty(),
        "no memory row when no memory file"
    );
    assert_eq!(
        v.measurement.section(SectionKind::Memory).unwrap().tokens,
        0
    );
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
    let v = b.build_for_turn(&[], None, None, &assembly_input(&tool_defs, None));
    let tools = v
        .measurement
        .section(SectionKind::Tools)
        .expect("Tools section present");
    assert!(
        tools.tokens > 0,
        "tool schema tokens counted: {}",
        tools.tokens
    );
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
    let v = b.build_for_turn(&[], None, None, &AssemblyInput::default());
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
    // Bare assembly must leave the cache alone: measuring a fresh session for
    // /context must not masquerade as a served turn measurement.
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
    // The memory file is a category of its own, not part of the prompt's
    // footprint, so the legend can show what the project rules cost.
    let memory = v
        .measurement
        .section(SectionKind::Memory)
        .expect("memory section");
    assert!(memory.tokens > 0, "the project memory file is charged");
    let rows = &v.measurement.memory_files;
    assert_eq!(rows.len(), 1, "one row per injected memory file");
    assert_eq!(rows[0].label, "AGENTS.md");
    assert_eq!(rows[0].tokens, memory.tokens, "rows sum to the section");
    assert!(
        memory.tokens < sys.tokens,
        "the prompt keeps its own framing, not the whole total"
    );
    remove_dir_all(&scratch).ok();
}

/// A lowercase memory file is a different file this project never reads.
/// Loading one would silently pick up an unrelated note the user happened to
/// name that way, so the walk-up only accepts the established spellings.
///
/// The name under test differs by more than case on purpose: a
/// case-insensitive filesystem resolves a case-only difference to the same
/// file, so only a genuinely different spelling proves the rule.
#[test]
fn test_lowercase_memory_file_ignored() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-lower-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    write(
        scratch.join("agent.md"),
        "# Lowercase Note\n\nnot a project rule.",
    )
    .expect("write");
    write(
        scratch.join("agent.local.md"),
        "# Lowercase Overlay\n\nnot a private overlay.",
    )
    .expect("write");
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let v = b.build(&[]);
    assert!(
        !v.system.contains("Lowercase Note"),
        "a lowercase agent.md is not a project memory file"
    );
    assert!(
        !v.system.contains("Lowercase Overlay"),
        "a lowercase agent.local.md is not a private overlay"
    );
    assert!(
        v.measurement.memory_files.is_empty(),
        "no memory row for a file the project does not read"
    );
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
    assert!(
        v.measurement.memory_files.is_empty(),
        "the behavior guidance is prompt framing, not a memory row"
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
    let rows = &v.measurement.memory_files;
    assert_eq!(
        rows.len(),
        1,
        "one aggregate row, not one row per recalled entry"
    );
    assert_eq!(rows[0].label, "Recalled memories (1 entry)");
    assert_eq!(rows[0].tokens, mem.tokens, "rows sum to the section");
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
    // The announced listing rides the user message; the Skills section charges
    // the same entries the announcement was built from, so the legend and the
    // drill-down rows come from one definition and the listing is not
    // double-counted.
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-skillsec-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let descriptor = SkillDescriptor {
        name: "commit".into(),
        description: "commit changes".into(),
        body_token_estimate: 0,
        ..sample_descriptor()
    };
    let listing_text = listing_entry(&descriptor);
    let snapshot = SkillSnapshot {
        descriptor,
        origin: "project".into(),
        usage: SkillUsage::default(),
    };
    let skills = [snapshot];
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
                text: listing_text.clone(),
                bytes: listing_text.len() as u32,
                content_hash: 0,
            },
        },
    ];
    let input = AssemblyInput {
        tool_defs: &[],
        memory_index: None,
        announced_skills: &skills,
        discovered_skills: &skills,
    };
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let v = b.measure(&events, None, None, &input);
    let skill = v
        .measurement
        .section(SectionKind::Skills)
        .expect("Skills section present when a skill is announced");
    assert!(
        skill.tokens > 0,
        "Skills section must count the listing entries"
    );
    assert_eq!(v.measurement.skills.len(), 1, "one row per announced entry");
    assert_eq!(v.measurement.skills[0].name, "commit");
    assert_eq!(v.measurement.skills[0].source, "project");
    assert_eq!(
        v.measurement.skills[0].tokens, skill.tokens,
        "rows sum to the section"
    );
    // No double-count: the assembly merges the listing text into the user
    // message, so Messages (non-listing input) + Skills (the entries) must
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

/// A skill the listing does not announce still gets a row: the /context drill-down
/// covers discovery, so a skill the model cannot invoke reads beside the
/// announced ones instead of vanishing, and its cost is zero because the
/// window carries nothing of it. The announced row keeps the section's whole
/// cost, so the rows still sum to the category the legend reports.
#[test]
fn test_skill_rows_cover_manual() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-skillmanual-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let listed = SkillSnapshot {
        descriptor: SkillDescriptor {
            name: "commit".into(),
            description: "commit changes".into(),
            body_token_estimate: 0,
            ..sample_descriptor()
        },
        origin: "project".into(),
        usage: SkillUsage::default(),
    };
    let listing_text = listing_entry(&listed.descriptor);
    let manual = SkillSnapshot {
        descriptor: SkillDescriptor {
            name: "brainstorm".into(),
            description: "run a brainstorm".into(),
            disable_model_invocation: true,
            body_token_estimate: 0,
            ..sample_descriptor()
        },
        origin: "claude_eco".into(),
        usage: SkillUsage::default(),
    };
    let announced = [listed];
    let discovered = [announced[0].clone(), manual];
    let events = vec![SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::SkillListing {
            text: listing_text,
            bytes: 0,
            content_hash: 0,
        },
    }];
    let input = AssemblyInput {
        tool_defs: &[],
        memory_index: None,
        announced_skills: &announced,
        discovered_skills: &discovered,
    };
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let v = b.measure(&events, None, None, &input);
    let section = v
        .measurement
        .section(SectionKind::Skills)
        .expect("Skills section present");
    let rows = &v.measurement.skills;
    assert_eq!(rows.len(), 2, "a row per discovered skill");
    let manual_row = rows
        .iter()
        .find(|row| row.name == "brainstorm")
        .expect("the skill the listing does not announce has a row");
    assert!(
        !manual_row.announced,
        "a skill the listing does not announce reports itself manual"
    );
    assert_eq!(
        manual_row.tokens, 0,
        "the window carries nothing of a skill the listing does not announce"
    );
    let listed_row = rows
        .iter()
        .find(|row| row.name == "commit")
        .expect("the announced skill has a row");
    assert!(listed_row.announced, "the announced skill reports itself");
    assert_eq!(
        listed_row.tokens, section.tokens,
        "the announced rows still sum to the section"
    );
    remove_dir_all(&scratch).ok();
}

/// A listing shortened to fit its budget costs less than the full entries, and
/// the section reports what the request carries: the rows split the measured
/// announcement rather than summing to a larger figure the prompt never holds.
#[test]
fn test_skill_section_follows_listing() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-skilltrunc-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let descriptor = SkillDescriptor {
        name: "commit".into(),
        description: "y".repeat(600),
        body_token_estimate: 0,
        ..sample_descriptor()
    };
    let full = listing_entry(&descriptor);
    // A window too small for any description keeps the names, the tightest
    // tier of the listing budget.
    let truncated = format_skill_listing(from_ref(&descriptor), 1_000);
    assert!(
        truncated.len() < full.len(),
        "the fixture must actually truncate: {truncated}"
    );
    let snapshot = SkillSnapshot {
        descriptor,
        origin: "project".into(),
        usage: SkillUsage::default(),
    };
    let skills = [snapshot];
    let events = vec![SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::SkillListing {
            text: truncated.clone(),
            bytes: truncated.len() as u32,
            content_hash: 0,
        },
    }];
    let input = AssemblyInput {
        tool_defs: &[],
        memory_index: None,
        announced_skills: &skills,
        discovered_skills: &skills,
    };
    let tok = Tokenizer::new();
    let v = ContextBuilder::new()
        .with_cwd(scratch.clone())
        .measure(&events, None, None, &input);
    let skill = v
        .measurement
        .section(SectionKind::Skills)
        .expect("Skills section present");
    assert_eq!(
        skill.tokens,
        tok.count(&truncated),
        "the section is the announcement the request carries"
    );
    assert!(
        skill.tokens < tok.count(&full),
        "and it is smaller than the untruncated entries"
    );
    assert_eq!(
        v.measurement
            .skills
            .iter()
            .map(|row| row.tokens)
            .sum::<u32>(),
        skill.tokens,
        "the rows split the section"
    );
    remove_dir_all(&scratch).ok();
}

/// Splitting a measured total across parts keeps the total exactly: floors,
/// then the largest remainders, with ties going to the earlier part so the
/// split is stable.
#[test]
fn test_split_total_sums_exact() {
    assert_eq!(split_total(10, &[1, 1]), vec![5, 5]);
    assert_eq!(split_total(7, &[1, 1, 1]), vec![3, 2, 2]);
    assert_eq!(split_total(100, &[3, 1]), vec![75, 25]);
    assert_eq!(
        split_total(5, &[0, 0]),
        vec![5, 0],
        "a zero measure reports"
    );
    assert_eq!(split_total(5, &[]), Vec::<u32>::new());
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
        system_prompt_tokens: 1_800,
        memory_files: vec![ContextFileEntry {
            label: "AGENTS.md".into(),
            tokens: 400,
        }],
        skills: Vec::new(),
        sections: vec![
            Section {
                kind: SectionKind::SystemPrompt,
                tokens: 1_800,
            },
            Section {
                kind: SectionKind::Messages,
                tokens: 30_000,
            },
        ],
        tool_footprints: Vec::new(),
        delegated: None,
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
    assert_eq!(
        bd.memory_files, measurement.memory_files,
        "the drill-down rows travel with the measurement that produced them"
    );
    assert_eq!(bd.skills, measurement.skills);
}

/// Wrap one event in a log entry, for the measurement cases that read a
/// sequence of events directly rather than through a session.
fn entry(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event,
    }
}

#[test]
fn test_footprint_counts_results() {
    let builder = ContextBuilder::new();
    let small_output = serde_json::json!("one line");
    let large_output = serde_json::json!("a whole file the model should not have read");
    let events = vec![
        entry(SessionEvent::ToolCall {
            call_id: "1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }),
        entry(SessionEvent::tool_result("1", large_output.clone())),
        entry(SessionEvent::ToolCall {
            call_id: "2".into(),
            tool: "read".into(),
            input: serde_json::json!({"path": "src/lib.rs"}),
        }),
        entry(SessionEvent::tool_result("2", small_output.clone())),
    ];
    let rows = builder.tool_footprints(&events);
    assert_eq!(rows.len(), 2, "one row per tool: {rows:?}");
    assert_eq!(rows[0].tool, "bash", "the heaviest total leads: {rows:?}");
    assert_eq!(rows[1].tool, "read");
    for row in &rows {
        assert_eq!(row.calls, 1);
        assert!(row.call_tokens > 0, "the call block is measured: {row:?}");
    }
    // Each result is counted against the call it answers, not against the
    // tool named by the position it arrived in.
    assert_eq!(
        rows[0].result_tokens,
        builder.tokenizer.count(&large_output.to_string()),
        "the bash row carries its own result"
    );
    assert_eq!(
        rows[1].result_tokens,
        builder.tokenizer.count(&small_output.to_string()),
        "the read row carries its own result"
    );
}

/// Two calls to one tool fold into one row, so the row shows what the tool cost
/// over the session rather than what one call cost.
#[test]
fn test_footprint_sums_calls() {
    let builder = ContextBuilder::new();
    let first = serde_json::json!("first output");
    let second = serde_json::json!("second output");
    let events = vec![
        entry(SessionEvent::ToolCall {
            call_id: "1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }),
        entry(SessionEvent::tool_result("1", first.clone())),
        entry(SessionEvent::ToolCall {
            call_id: "2".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "pwd"}),
        }),
        entry(SessionEvent::tool_result("2", second.clone())),
    ];
    let rows = builder.tool_footprints(&events);
    assert_eq!(rows.len(), 1, "one row for the tool: {rows:?}");
    assert_eq!(rows[0].calls, 2);
    assert_eq!(
        rows[0].result_tokens,
        builder.tokenizer.count(&first.to_string()) + builder.tokenizer.count(&second.to_string()),
        "both results are counted"
    );
}

#[test]
fn test_footprint_ignores_orphan() {
    // A result whose call the selection dropped has no tool to attribute, so it
    // is counted under no tool rather than guessed from the result body.
    let builder = ContextBuilder::new();
    let events = vec![entry(SessionEvent::tool_result(
        "gone",
        serde_json::json!("output of a call that is no longer selected"),
    ))];
    assert!(
        builder.tool_footprints(&events).is_empty(),
        "an unattributable result produces no row"
    );
}

#[test]
fn test_delegated_reports_return() {
    let builder = ContextBuilder::new();
    let summary = "the child found three call sites";
    let events = vec![entry(SessionEvent::SubagentReturn {
        child_session_id: "child".into(),
        status: "completed".into(),
        summary: summary.into(),
        result_ref: String::new(),
        input_tokens: 900,
        output_tokens: 100,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        input_reported: true,
        cache_read_reported: true,
        cache_write_reported: true,
    })];
    let delegated = builder
        .delegated_context(&events)
        .expect("a session that delegated reports it");
    assert_eq!(delegated.returns, 1);
    assert_eq!(
        delegated.returned_tokens,
        builder.tokenizer.count(summary),
        "the return cost is the summary the session kept"
    );
    assert_eq!(
        delegated.child_tokens,
        Some(1_000),
        "input plus output is what the child processed"
    );
}

#[test]
fn test_delegated_unknown_total() {
    // A child that reported nothing leaves its total unknown: the parent knows
    // it delegated, not what the child spent.
    let builder = ContextBuilder::new();
    let events = vec![entry(SessionEvent::SubagentReturn {
        child_session_id: "child".into(),
        status: "completed".into(),
        summary: "no usage reported".into(),
        result_ref: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        input_reported: false,
        cache_read_reported: false,
        cache_write_reported: false,
    })];
    let delegated = builder
        .delegated_context(&events)
        .expect("the return is still reported");
    assert_eq!(delegated.returns, 1);
    assert_eq!(
        delegated.child_tokens, None,
        "an unreported total stays unknown rather than reading zero"
    );
}

#[test]
fn test_delegated_absent() {
    let builder = ContextBuilder::new();
    let events = vec![entry(SessionEvent::UserInput {
        text: "no delegation here".into(),
    })];
    assert!(
        builder.delegated_context(&events).is_none(),
        "a session with no returns has no delegated section"
    );
}

/// The conversation cost is derived for the /context measurement and not for a
/// turn. Deriving it counts a token per tool call input and per tool result
/// across the whole history, so a turn that only reports the occupancy must not
/// pay it, while the pane that shows the footprints must.
#[test]
fn test_turn_skips_conversation_cost() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-turncost-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let events = vec![
        entry(SessionEvent::ToolCall {
            call_id: "1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }),
        entry(SessionEvent::tool_result(
            "1",
            serde_json::json!("a whole directory listing the model paid for"),
        )),
        entry(SessionEvent::SubagentReturn {
            child_session_id: "child".into(),
            status: "completed".into(),
            summary: "the child found three call sites".into(),
            result_ref: String::new(),
            input_tokens: 900,
            output_tokens: 100,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: true,
        }),
    ];
    let turn = b.build_for_turn(&events, None, None, &AssemblyInput::default());
    assert!(
        turn.measurement.tool_footprints.is_empty(),
        "a turn reports no tool footprint"
    );
    assert!(
        turn.measurement.delegated.is_none(),
        "a turn reports no delegated spend"
    );
    let pane = b.measure(&events, None, None, &AssemblyInput::default());
    assert_eq!(
        pane.measurement.tool_footprints.len(),
        1,
        "the pane measures the session's tool cost"
    );
    assert!(
        pane.measurement.delegated.is_some(),
        "the pane measures the session's delegated spend"
    );
    remove_dir_all(&scratch).ok();
}

/// A skills section holding only the entries the next request would carry is a
/// listing no message stream carries yet, so it takes nothing from the messages.
/// Charging the messages for it would report a stream smaller than the one the
/// session holds.
#[test]
fn test_skills_estimate_keeps_messages() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-skillest-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let skills = [SkillSnapshot {
        descriptor: SkillDescriptor {
            name: "commit".into(),
            description: "commit changes".into(),
            body_token_estimate: 0,
            ..sample_descriptor()
        },
        origin: "project".into(),
        usage: SkillUsage::default(),
    }];
    let events = vec![entry(SessionEvent::UserInput {
        text: "help me commit".into(),
    })];
    let input = AssemblyInput {
        tool_defs: &[],
        memory_index: None,
        announced_skills: &skills,
        discovered_skills: &skills,
    };
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let v = b.measure(&events, None, None, &input);
    let tok = Tokenizer::new();
    assert_eq!(
        v.measurement.section(SectionKind::Messages).unwrap().tokens,
        tok.count("help me commit"),
        "the messages section reports the whole stream"
    );
    let skill = v
        .measurement
        .section(SectionKind::Skills)
        .expect("skills section present");
    assert!(
        skill.tokens > 0,
        "the entries alone are what the next request would carry"
    );
    assert_eq!(v.measurement.skills.len(), 1, "one row per entry");
    assert_eq!(
        v.measurement.skills[0].tokens, skill.tokens,
        "rows sum to the section"
    );
    remove_dir_all(&scratch).ok();
}

/// The listing a message stream carries is charged to the skills section and
/// taken from the messages even when no skill rows describe it: what is
/// subtracted is what the stream holds, not what the rows can name.
#[test]
fn test_listing_charged_without_rows() {
    let mut scratch = temp_dir();
    scratch.push(format!("ctx-test-skillnorow-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let listing = "commit: commit changes";
    let events = vec![
        entry(SessionEvent::UserInput {
            text: "help me commit".into(),
        }),
        entry(SessionEvent::SkillListing {
            text: listing.into(),
            bytes: listing.len() as u32,
            content_hash: 0,
        }),
    ];
    let b = ContextBuilder::new().with_cwd(scratch.clone());
    let v = b.measure(&events, None, None, &AssemblyInput::default());
    let tok = Tokenizer::new();
    let merged = format!("help me commit\n{listing}");
    let full_input = tok.count(&merged);
    let messages = v.measurement.section(SectionKind::Messages).unwrap().tokens;
    assert_eq!(
        messages,
        full_input - tok.count(listing),
        "the listing rides the stream, so the messages section is charged for it"
    );
    assert_eq!(
        v.measurement.section(SectionKind::Skills).unwrap().tokens,
        tok.count(listing),
        "the skills section reports the listing the stream carries"
    );
    assert!(
        v.measurement.skills.is_empty(),
        "no configured skill describes the listing, so there is no row to name it"
    );
    remove_dir_all(&scratch).ok();
}

#[test]
fn test_build_empty_events_view() {
    // build(&[]) with no events is the empty-session path: a session with
    // nothing loaded still reports the prompt and its capability sections.
    let cb = ContextBuilder::new();
    let view = cb.build(&[]);
    assert!(
        view.measurement.token_count() > 0,
        "an empty session must have system prompt tokens, not 0"
    );
    let messages = view
        .measurement
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Messages)
        .expect("messages section present with no events");
    assert_eq!(
        messages.tokens, 0,
        "messages tokens must be 0 (no turn run)"
    );
    let bd = view.measurement.breakdown("test", 200_000);
    assert!(
        bd.categories.len() >= 3,
        "the breakdown has a category per section plus free space"
    );
    assert!(!bd.grid.is_empty(), "the breakdown builds a grid");
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
    let memory = v
        .measurement
        .section(SectionKind::Memory)
        .expect("memory section");
    let rows = &v.measurement.memory_files;
    assert_eq!(rows.len(), 1, "the recall is the only memory source here");
    assert!(
        !rows[0].label.contains("DEPLOY_COMMAND"),
        "the recalled content stays out of the row labels"
    );
    assert_eq!(
        memory.tokens,
        Tokenizer::new().count(&recall_text),
        "Memory counts the recalled attachment"
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

    let a = ContextBuilder::new().with_cwd(dir_a.clone()).measure(
        &[],
        None,
        None,
        &assembly_input(&[], Some(index)),
    );
    let b = ContextBuilder::new().with_cwd(dir_b.clone()).measure(
        &[],
        None,
        None,
        &assembly_input(&[], Some(index)),
    );

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

/// The installed definitions are the stored fact; the prompt paragraph is
/// derived from them at assembly. One store means what the model is told and
/// what the /agents reply carries cannot name different type sets.
#[test]
fn test_agent_types_drive_section() {
    let empty = ContextBuilder::new().measure(&[], None, None, &assembly_input(&[], None));
    assert!(
        !empty.system.contains("Available agents"),
        "no installed definitions means no directory section"
    );

    let cb = ContextBuilder::new();
    cb.set_agent_types(vec![built_in_general_purpose()]);
    assert_eq!(
        cb.agent_types().len(),
        1,
        "the store reads back what it holds"
    );
    let full = cb.measure(&[], None, None, &assembly_input(&[], None));
    assert!(
        full.system.contains("## Available agents"),
        "an installed definition reaches the assembled prompt"
    );
    assert!(
        full.system.contains("- general-purpose:"),
        "the section renders the definition it was derived from"
    );
}

/// A measurement must read the tool definitions the caller declares: a
/// /context that names no tools reports no tool footprint, so the pane cannot
/// claim a cost the request does not pay.
#[test]
fn test_measure_reports_declared_tools() {
    let cb = ContextBuilder::new();
    let tool_defs = vec![ToolDef {
        name: "bash".to_string(),
        description: "Run a shell command in the workspace.".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    let with_tools = cb.measure(&[], None, None, &assembly_input(&tool_defs, None));
    let tools = with_tools
        .measurement
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Tools)
        .expect("tools section present");
    assert!(
        tools.tokens > 0,
        "the tools section must measure the real definitions"
    );

    let without = cb.measure(&[], None, None, &AssemblyInput::default());
    let tools = without
        .measurement
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Tools)
        .expect("the tools section keeps its place in the order");
    assert_eq!(tools.tokens, 0, "no declared tool, no tool footprint");
    assert!(
        cb.last_measurement().is_none(),
        "measuring for /context must not populate the turn cache"
    );
}

/// A worktree switch must not rewrite the prompt prefix.
///
/// Entering a worktree changes the cwd, and the cwd drives the project-memory
/// walk-up. Re-reading it mid-session would change the system prompt under a
/// live cache and invalidate the whole cached context, so the session holds
/// what it read first and refreshes only at an explicit boundary.
#[test]
fn test_cwd_switch_keeps_prompt() {
    let mut root = temp_dir();
    root.push(format!("ctx-test-freeze-{}", id()));
    let worktree = root.join("worktree");
    create_dir_all(&worktree).expect("mkdir worktree");
    write(root.join("AGENTS.md"), "# Main Rules\n\nmain rules.").expect("write main");
    write(
        worktree.join("AGENTS.md"),
        "# Worktree Rules\n\nworktree rules.",
    )
    .expect("write worktree");

    let b = ContextBuilder::new().with_cwd(root.clone());
    let first = b.build(&[]).system;
    assert!(
        first.contains("Main Rules"),
        "the main rules are read first"
    );

    b.switch_cwd(worktree.clone());
    let after_switch = b.build(&[]).system;
    assert_eq!(
        after_switch, first,
        "a worktree switch must not rewrite the prompt prefix"
    );

    // The explicit boundary does refresh it.
    b.reset_frozen_prompt();
    let after_reset = b.build(&[]).system;
    assert!(
        after_reset.contains("Worktree Rules"),
        "an explicit boundary re-reads the project memory:\n{after_reset}"
    );
    remove_dir_all(&root).ok();
}
