//! Bounded expansion of read-only shell loops and conditional clauses.
//!
//! Diagnostic shell snippets often iterate a fixed word list with a loop and
//! test each path inside a conditional branch. Rather than guessing what an
//! unexpanded loop variable might name, this module unrolls small literal
//! loops into concrete segments so every downstream read-only and credential
//! rule judges the exact arguments the shell will pass.

use super::super::{decode_shell_word, shell_word_spans};
use crate::pipeline::detection::strip_quotes;

/// Maximum candidate count a single loop header may list.
const MAX_LOOP_ITEMS: usize = 16;

/// Maximum concrete segments produced after unrolling nested loops.
const MAX_EXPANDED_SEGMENTS: usize = 64;

/// Maximum nested loop depth accepted for static unrolling.
const MAX_LOOP_NESTING: usize = 2;

struct LoopBinding {
    name: String,
    values: Vec<String>,
    derived: Vec<(String, Vec<String>)>,
}

/// Expand literal loop and conditional structure across compound segments,
/// returning concrete command segments ready for per-segment classification.
/// Returns None when a loop header uses dynamic expansion, exceeds the
/// unrolling bound, or leaves an unclosed block.
pub(crate) fn expand_readonly_clauses(segments: &[&str]) -> Option<Vec<String>> {
    let mut scopes: Vec<LoopBinding> = Vec::new();
    let mut expanded: Vec<String> = Vec::new();

    for &raw_seg in segments {
        let seg = peel_leading_keywords(raw_seg.trim());
        if seg.is_empty() || seg == "fi" {
            continue;
        }
        if seg == "done" {
            scopes.pop()?;
            continue;
        }
        if let Some(binding) = parse_for_header(seg) {
            if scopes.len() >= MAX_LOOP_NESTING {
                return None;
            }
            scopes.push(binding);
            continue;
        }
        if let Some(active) = scopes.last_mut()
            && let Some((derived_name, derived_vals)) = parse_tmpdir_derivation(seg, active)
        {
            active.derived.push((derived_name, derived_vals));
            continue;
        }
        let concrete = instantiate_scopes(seg, &scopes)?;
        if expanded.len().saturating_add(concrete.len()) > MAX_EXPANDED_SEGMENTS {
            return None;
        }
        expanded.extend(concrete);
    }

    if !scopes.is_empty() || expanded.is_empty() {
        return None;
    }
    Some(expanded)
}

/// Strip leading conditional and loop body keywords from the front of a
/// segment.
fn peel_leading_keywords(mut seg: &str) -> &str {
    loop {
        let trimmed = seg.trim_start();
        let Some(end) = trimmed.find(char::is_whitespace) else {
            return trimmed;
        };
        let head = &trimmed[..end];
        if matches!(head, "do" | "if" | "then" | "elif" | "else") {
            seg = &trimmed[end..];
        } else {
            return trimmed;
        }
    }
}

/// Parse a literal for-in loop header over plain word items.
fn parse_for_header(seg: &str) -> Option<LoopBinding> {
    let spans = shell_word_spans(seg);
    if spans.len() < 4 || spans[0] != "for" || spans[2] != "in" {
        return None;
    }
    let var = spans[1];
    if !is_safe_loop_identifier(var) {
        return None;
    }
    let item_spans = &spans[3..];
    if item_spans.is_empty() || item_spans.len() > MAX_LOOP_ITEMS {
        return None;
    }
    let mut values = Vec::with_capacity(item_spans.len());
    for &raw in item_spans {
        if raw.contains(['*', '?', '[', ']', '<', '>', '(', ')']) {
            return None;
        }
        let decoded = decode_shell_word(raw)?;
        if decoded.is_empty() || decoded.chars().any(char::is_whitespace) {
            return None;
        }
        values.push(decoded);
    }
    Some(LoopBinding {
        name: var.to_string(),
        values,
        derived: Vec::new(),
    })
}

/// Recognize a temporary-directory path derivation inside a loop over plain
/// child file names so log-inspection loops can address session temporary
/// files.
fn parse_tmpdir_derivation(seg: &str, scope: &LoopBinding) -> Option<(String, Vec<String>)> {
    let spans = shell_word_spans(seg);
    if spans.len() != 1 {
        return None;
    }
    let (lhs, rhs_raw) = spans[0].split_once('=')?;
    if !is_safe_loop_identifier(lhs) || lhs == scope.name {
        return None;
    }
    let rhs = strip_quotes(rhs_raw);
    let plain_pat = format!("$TMPDIR/${}", scope.name);
    let braced_pat = format!("${{TMPDIR}}/${{{}}}", scope.name);
    let mixed_a = format!("${{TMPDIR}}/${}", scope.name);
    let mixed_b = format!("$TMPDIR/${{{}}}", scope.name);
    if rhs != plain_pat && rhs != braced_pat && rhs != mixed_a && rhs != mixed_b {
        return None;
    }
    if !scope.values.iter().all(|v| is_plain_child_name(v)) {
        return None;
    }
    let mapped = scope
        .values
        .iter()
        .map(|v| format!("$TMPDIR/{v}"))
        .collect();
    Some((lhs.to_string(), mapped))
}

fn is_plain_child_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains([
            '/', '\\', '*', '?', '[', ']', '$', '`', '(', ')', ';', '|', '&',
        ])
}

fn is_safe_loop_identifier(name: &str) -> bool {
    if name.is_empty()
        || matches!(
            name,
            "TMPDIR"
                | "TMPPREFIX"
                | "PATH"
                | "IFS"
                | "HOME"
                | "BASH_ENV"
                | "ENV"
                | "CDPATH"
                | "GLOBIGNORE"
                | "SHELLOPTS"
                | "BASHOPTS"
        )
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("GIT_")
    {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn instantiate_scopes(seg: &str, scopes: &[LoopBinding]) -> Option<Vec<String>> {
    let mut current = vec![seg.to_string()];
    for scope in scopes {
        let mut next = Vec::with_capacity(current.len().saturating_mul(scope.values.len()));
        for template in &current {
            for (idx, val) in scope.values.iter().enumerate() {
                let mut rendered = replace_shell_var(template, &scope.name, val);
                for (d_name, d_vals) in &scope.derived {
                    if let Some(d_val) = d_vals.get(idx) {
                        rendered = replace_shell_var(&rendered, d_name, d_val);
                    }
                }
                next.push(rendered);
                if next.len() > MAX_EXPANDED_SEGMENTS {
                    return None;
                }
            }
        }
        current = next;
    }
    Some(current)
}

/// Replace dollar-prefixed and brace-wrapped occurrences of name outside
/// single quotes when name is not followed by another identifier character.
fn replace_shell_var(text: &str, name: &str, replacement: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let name_chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(text.len() + replacement.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' && !in_double {
            in_single = !in_single;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            out.push(c);
            i += 1;
            continue;
        }
        if in_double && c == '\\' {
            out.push(c);
            if let Some(&next) = chars.get(i + 1) {
                out.push(next);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if !in_single && c == '$' {
            if chars.get(i + 1) == Some(&'{') {
                let start = i + 2;
                let end = start + name_chars.len();
                if chars.get(start..end) == Some(name_chars.as_slice())
                    && chars.get(end) == Some(&'}')
                {
                    out.push_str(replacement);
                    i = end + 1;
                    continue;
                }
            } else {
                let start = i + 1;
                let end = start + name_chars.len();
                let boundary_ok = chars
                    .get(end)
                    .is_none_or(|ch| !(ch.is_ascii_alphanumeric() || *ch == '_'));
                if chars.get(start..end) == Some(name_chars.as_slice()) && boundary_ok {
                    out.push_str(replacement);
                    i = end;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}
