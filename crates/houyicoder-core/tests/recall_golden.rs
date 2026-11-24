//! Reproducible before-and-after golden metrics over the 200-entry independent
//! corpus and the 182-entry real snapshot backup when present locally.

use std::collections::HashSet;
use std::env::temp_dir;
use std::fs::{self, create_dir_all, remove_dir_all};
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::path::home_directory;
use houyicoder_context::{MemoryScope, MemoryType, is_cjk, tokens_for};
use houyicoder_core::agent::render_child_memory;
use houyicoder_memory::MarkdownMemoryProvider;

use crate::recall_corpus::{discard_root, load_corpus, seed_corpus};

fn tokenize_legacy(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut buf_cjk = false;
    for c in query.chars() {
        if !c.is_alphanumeric() {
            flush_tok(&mut buf, buf_cjk, &mut out);
            buf_cjk = false;
            continue;
        }
        let cjk = is_cjk(c);
        if cjk != buf_cjk && !buf.is_empty() {
            flush_tok(&mut buf, buf_cjk, &mut out);
        }
        buf_cjk = cjk;
        buf.push(c);
    }
    flush_tok(&mut buf, buf_cjk, &mut out);
    out
}

fn flush_tok(buf: &mut String, cjk: bool, out: &mut Vec<String>) {
    if buf.is_empty() {
        return;
    }
    let lower = buf.to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    if chars.len() >= 2 {
        if cjk {
            for w in chars.windows(2) {
                out.push(w.iter().collect());
            }
            if chars.len() > 2 {
                out.push(lower);
            }
        } else {
            out.push(lower);
        }
    }
    buf.clear();
}

/// Simulate pre-hardening frontmatter-only lexical rank (key + first line or
/// uncapped description only, body ignored).
fn legacy_frontmatter_top5(roots: &[PathBuf], query: &str) -> Vec<String> {
    let kws = tokenize_legacy(query);
    if kws.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, String)> = Vec::new();
    for root in roots {
        let Ok(rd) = fs::read_dir(root) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md")
                || p.file_name().and_then(|n| n.to_str()) == Some("MEMORY.md")
            {
                continue;
            }
            let key = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let Ok(txt) = fs::read_to_string(&p) else {
                continue;
            };
            let mut desc = String::new();
            let parts: Vec<&str> = txt.splitn(3, "---").collect();
            if parts.len() >= 3 {
                for line in parts[1].lines() {
                    if let Some(rest) = line.trim().strip_prefix("description:") {
                        desc = rest.trim().to_string();
                    }
                }
            }
            let hay = format!("{key} {desc}").to_ascii_lowercase();
            let hits = kws.iter().filter(|k| hay.contains(k.as_str())).count();
            if hits >= 2 {
                scored.push((hits, key));
            }
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(5).map(|(_, k)| k).collect()
}

fn copy_snapshot_to_temp(src: &Path) -> (PathBuf, Vec<PathBuf>) {
    let dst = temp_dir().join(format!("golden-snap-{}", id()));
    drop(remove_dir_all(&dst));
    let mut roots = Vec::new();
    for scope in ["user", "project", "auto"] {
        let s_dir = src.join(scope);
        let d_dir = dst.join(scope);
        create_dir_all(&d_dir).expect("mkdir");
        for ent in fs::read_dir(&s_dir).expect("read snapshot scope").flatten() {
            let p = ent.path();
            if p.is_file() {
                let bytes = fs::read(&p).expect("read file");
                fs::write(d_dir.join(ent.file_name()), bytes).expect("write writable copy");
            }
        }
        roots.push(d_dir);
    }
    (dst, roots)
}

fn body_entity_queries() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "why is hook_commit_gate.py blocking git commit without .commit_ready",
            "feedback-commit-protocol-full-chain",
        ),
        (
            "how to normalize windows paths with dunce::canonicalize before push",
            "feedback-prepush-crossplatform-verification",
        ),
        (
            "git push hangs unless GIT_SSH_COMMAND and stdin /dev/null are set",
            "feedback-push-standard-procedure",
        ),
        (
            "activate worktree hooks with hook_guard.sh and .rs-comment-products",
            "feedback-hook-activation-surface",
        ),
        (
            "target/debug/.cargo-lock denied by seatbelt profile in probe-copy.json",
            "project-sandbox-seatbelt-deny-patterns",
        ),
        (
            "approval/scratch_local.rs SAFE_TAR_BUNDLE advance_scratch repair",
            "project-egress-cwd-repair-progress-2026-11",
        ),
        (
            "do not use python script to rewrite ratatui::style::Color or serde_json",
            "feedback-prefer-edit-tool-over-scripts",
        ),
        (
            "check .git/MERGE_HEAD or exit_worktree before handing refusal to user",
            "feedback-classify-refusal-before-handing-back",
        ),
        (
            "did parent commit 9621bd89 already support Event::Paste",
            "feedback-attribute-change-by-diffing-parent",
        ),
        (
            "tokio::select! dropped biased modifier during module extraction",
            "lesson-extraction-drops-untyped-modifiers",
        ),
        (
            "schema error write: content (string) required on file tool call",
            "lesson-missing-required-argument-on-write",
        ),
        (
            "wiki handoff requires checking STATUS.md and calling exit_worktree",
            "feedback-wiki-handoff-shape",
        ),
    ]
}

fn cjk_and_negative_queries() -> (Vec<(&'static str, &'static str)>, Vec<&'static str>) {
    let cjk = vec![
        (
            "为什么刚推的提交CI变红要先查更早的提交",
            "lesson-ci-red-on-your-commit-may-predate-it",
        ),
        (
            "收紧权限分类器为什么要准备双向正反例语料",
            "lesson-classifier-tightening-needs-both-directions",
        ),
        (
            "报出测试覆盖率之前为什么要先查环境污染",
            "lesson-coverage-readout-needs-clean-environment",
        ),
        (
            "工具输出路径看着不对且串联cd失败怎么定案",
            "lesson-harness-path-alias-worktree",
        ),
        (
            "提交说明里为什么禁止写抽象名词",
            "feedback-commit-message-banned-words",
        ),
        (
            "推送主分支之后为什么要立刻切回开发分支",
            "feedback-return-to-dev-after-main-push",
        ),
        (
            "改代码为什么优先用编辑工具而不是脚本替换",
            "feedback-prefer-edit-tool-over-scripts",
        ),
        (
            "等待长任务为什么不能起后台就结束当前轮次",
            "feedback-wait-with-one-self-terminating-loop",
        ),
    ];
    let negatives = vec![
        "how do I configure kubernetes ingress tls certificates in helm",
        "write a quicksort implementation over generic slices",
        "what is the boiling point of ethanol at sea level",
        "postgres vacuum freeze autovacuum tuning parameters",
        "react useeffect cleanup dependency array infinite loop",
        "ffmpeg h264 crf encoding preset comparison",
        "docker compose bridge network dns resolution",
        "graphql federation subgraph schema stitching",
        "蓝牙耳机连接断断续续怎么排查信号干扰",
        "今天晚饭做番茄牛腩需要炖多少分钟",
    ];
    (cjk, negatives)
}

#[test]
#[ignore]
fn test_real_snapshot_golden() {
    let _io_serial = crate::BENCH_IO_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let corpus = load_corpus();
    let (corpus_provider, corpus_root) = seed_corpus(&corpus);
    let mut en_hits = 0usize;
    let en_pairs = corpus.pairs_in("en_lexical");
    for pair in &en_pairs {
        let ranked = corpus_provider.rank_candidates(&pair.query, &HashSet::new());
        let top5: Vec<&str> = ranked
            .iter()
            .filter(|h| h.score >= 2)
            .take(5)
            .map(|h| h.key.as_str())
            .collect();
        if pair
            .expected
            .as_deref()
            .is_some_and(|exp| top5.contains(&exp))
        {
            en_hits += 1;
        }
    }
    let cjk_pairs = corpus.pairs_in("cjk_lexical");
    let mut cjk_corpus_hits = 0usize;
    for pair in &cjk_pairs {
        let ranked = corpus_provider.rank_candidates(&pair.query, &HashSet::new());
        let top5: Vec<&str> = ranked
            .iter()
            .filter(|h| h.score >= 2)
            .take(5)
            .map(|h| h.key.as_str())
            .collect();
        if pair
            .expected
            .as_deref()
            .is_some_and(|exp| top5.contains(&exp))
        {
            cjk_corpus_hits += 1;
        }
    }
    let neg_pairs = corpus.pairs_in("negative");
    let mut false_pos_corpus = 0usize;
    for pair in &neg_pairs {
        let ranked = corpus_provider.rank_candidates(&pair.query, &HashSet::new());
        if ranked.first().is_some_and(|h| h.score >= 2) {
            false_pos_corpus += 1;
        }
    }
    println!(
        "[golden:corpus200] step0_en={}/{} step0_cjk={}/{} false_inject={}/{}",
        en_hits,
        en_pairs.len(),
        cjk_corpus_hits,
        cjk_pairs.len(),
        false_pos_corpus,
        neg_pairs.len()
    );
    discard_root(&corpus_root);

    let Some(home) = home_directory() else {
        return;
    };
    let snap = home.join(".houyicoder/backups/pre-s54-snapshot");
    if !snap.join("auto").is_dir() {
        println!(
            "[golden:snapshot] skipped: {} absent on this machine",
            snap.display()
        );
        return;
    }
    measure_real_snapshot(&snap);
}

fn measure_real_snapshot(snap: &Path) {
    let (tmp_dir, roots) = copy_snapshot_to_temp(snap);
    let legacy_auto_idx = fs::read_to_string(roots[2].join("MEMORY.md")).unwrap_or_default();
    let legacy_auto_bytes = legacy_auto_idx.len();
    let legacy_auto_rows = legacy_auto_idx
        .lines()
        .filter(|l| l.starts_with("- "))
        .count();

    let heal_start = Instant::now();
    let provider = Arc::new(MarkdownMemoryProvider::new_multi(roots.clone()));
    provider.rebuild_if_stale().expect("self-heal");
    let heal_us = heal_start.elapsed().as_micros();

    let healed_auto_idx = fs::read_to_string(roots[2].join("MEMORY.md")).expect("healed auto idx");
    let healed_auto_bytes = healed_auto_idx.len();
    let healed_auto_rows = healed_auto_idx
        .lines()
        .filter(|l| l.starts_with("- "))
        .count();
    let healed_truncated = healed_auto_idx.contains("WARNING: index too long");

    let all_summaries = provider.list_memories();
    let child_block = render_child_memory(provider.as_ref()).unwrap_or_default();
    let idx_part = child_block
        .split("## Memory index\n\n")
        .nth(1)
        .unwrap_or("");
    let rules_part = child_block
        .split("## Memory index")
        .next()
        .unwrap_or("")
        .trim();
    let prompt_idx_rows = idx_part.lines().filter(|l| l.starts_with("- ")).count();
    let prompt_idx_bytes = idx_part.len();
    let prompt_idx_tokens = tokens_for(idx_part);
    let active_rule_cards = rules_part.lines().filter(|l| l.starts_with("- [")).count();
    let active_rule_tokens = tokens_for(rules_part);

    println!(
        "[golden:snapshot182] total_topics={} legacy_auto_MEMORY={}B({}/145 rows) -> healed_auto_MEMORY={}B({}/145 rows, truncated={}) heal_us={}",
        all_summaries.len(),
        legacy_auto_bytes,
        legacy_auto_rows,
        healed_auto_bytes,
        healed_auto_rows,
        healed_truncated,
        heal_us
    );
    println!(
        "[golden:prompt] prompt_index_rows={}/{} prompt_index_bytes={}B prompt_index_tokens={} active_rule_cards={} active_rule_tokens={}",
        prompt_idx_rows,
        all_summaries.len(),
        prompt_idx_bytes,
        prompt_idx_tokens,
        active_rule_cards,
        active_rule_tokens
    );

    assert!(!healed_truncated);
    assert_eq!(healed_auto_rows, 145);

    measure_snapshot_recall(&roots, provider.as_ref());
    verify_reconcile_and_archive(&roots, provider.as_ref());
    drop(remove_dir_all(&tmp_dir));
}

fn measure_snapshot_recall(roots: &[PathBuf], provider: &MarkdownMemoryProvider) {
    let body_qs = body_entity_queries();
    let mut leg_body_hits = 0usize;
    let mut new_body_hits = 0usize;
    for (q, exp) in &body_qs {
        if legacy_frontmatter_top5(roots, q).iter().any(|k| k == exp) {
            leg_body_hits += 1;
        }
        let ranked = provider.rank_candidates(q, &HashSet::new());
        if ranked
            .iter()
            .filter(|h| h.score >= 2)
            .take(5)
            .any(|h| h.key == *exp)
        {
            new_body_hits += 1;
        }
    }

    let (cjk_qs, neg_qs) = cjk_and_negative_queries();
    let mut leg_cjk_hits = 0usize;
    let mut new_cjk_hits = 0usize;
    for (q, exp) in &cjk_qs {
        if legacy_frontmatter_top5(roots, q).iter().any(|k| k == exp) {
            leg_cjk_hits += 1;
        }
        let ranked = provider.rank_candidates(q, &HashSet::new());
        if ranked
            .iter()
            .filter(|h| h.score >= 2)
            .take(5)
            .any(|h| h.key == *exp)
        {
            new_cjk_hits += 1;
        }
    }
    let mut false_inject = 0usize;
    for q in &neg_qs {
        let ranked = provider.rank_candidates(q, &HashSet::new());
        if ranked.first().is_some_and(|h| h.score >= 2) {
            false_inject += 1;
        }
    }
    println!(
        "[golden:recall] body_entity_recall@5: legacy={}/{} new={}/{} | cjk_recall@5: legacy={}/{} new={}/{} | neg_false_inject={}/{}",
        leg_body_hits,
        body_qs.len(),
        new_body_hits,
        body_qs.len(),
        leg_cjk_hits,
        cjk_qs.len(),
        new_cjk_hits,
        cjk_qs.len(),
        false_inject,
        neg_qs.len()
    );
    assert!(new_body_hits > leg_body_hits);
    assert_eq!(false_inject, 0);
}

fn verify_reconcile_and_archive(roots: &[PathBuf], provider: &MarkdownMemoryProvider) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rec_start = Instant::now();
    let initial_changes = provider.reconcile_scopes(now);
    let rec_initial_us = rec_start.elapsed().as_micros();

    let rec_idle_start = Instant::now();
    let noop_changes = provider.reconcile_scopes(now);
    let rec_idle_us = rec_idle_start.elapsed().as_micros();

    let auto_feedback_keys: Vec<String> = provider
        .list_memories()
        .into_iter()
        .filter(|s| s.scope == MemoryScope::Auto && s.memory_type == MemoryType::Feedback)
        .take(3)
        .map(|s| s.key)
        .collect();
    assert_eq!(auto_feedback_keys.len(), 3);
    for rule in &auto_feedback_keys {
        provider.record_gate_violation(rule);
        provider.record_gate_violation(rule);
    }
    let rec_active_start = Instant::now();
    let promo_changes = provider.reconcile_scopes(now);
    let rec_promo_us = rec_active_start.elapsed().as_micros();

    println!(
        "[golden:reconcile] pre_stats_promo={}({}us) | idle_changes={}({}us) | 3_gate_promo_changes={}({}us)",
        initial_changes.len(),
        rec_initial_us,
        noop_changes.len(),
        rec_idle_us,
        promo_changes.len(),
        rec_promo_us
    );
    assert_eq!(noop_changes.len(), 0);
    assert_eq!(promo_changes.len(), 3);
    let promoted_key = &auto_feedback_keys[0];
    assert!(roots[1].join(format!("{promoted_key}.md")).is_file());
    MemoryProvider::archive_memory(provider, promoted_key, MemoryScope::Project).expect("archive");
    MemoryProvider::restore_memory(provider, promoted_key, MemoryScope::Project).expect("restore");
    assert!(roots[1].join(format!("{promoted_key}.md")).is_file());
}
