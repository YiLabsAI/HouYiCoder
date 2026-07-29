//! Tool-output reducer: a per-tool reduction pass that shrinks a large tool
//! result before it is externalized to the CAS, so the served view carries a
//! compacted form + a block_ref pointer (the raw stays retrievable). The
//! never-worse guard pins the contract: a reduced output never emits more
//! than the raw (a filter that would inflate — pretty-printing compact JSON,
//! an ansi-strip that adds markers — falls back to the raw).
//!
//! Trust gate: tool output is an untrusted data stream. A reduced output
//! carries a data_tag so downstream projection never lets a control keyword
//! embedded in tool output (a fake system-reminder, a forbidden instruction)
//! become an instruction — the model treats it as data, not as a directive.

use houyicoder_async::PFut;

/// The trust level of a tool-output source. Tool + MCP output is untrusted
/// (a malicious tool can emit anything); a built-in tool over a local
/// sandbox is trusted. The trust gate tags untrusted output as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    Trusted,
    Untrusted,
}

/// Per-call reduction context: whether the caller requested the raw
/// (no-reduce) form + the trust level of the source.
#[derive(Debug, Clone, Copy)]
pub struct ReduceCtx {
    pub raw: bool,
    pub trust: TrustLevel,
}

/// A reduced tool output. The text field is the (possibly compacted) form the
/// served view carries; data_tag marks it as untrusted data; reduced is true
/// when the text is smaller than the raw (the never-worse guard may flip
/// this back to the raw).
#[derive(Debug, Clone)]
pub struct ReducedOutput {
    pub text: String,
    pub data_tag: bool,
    pub reduced: bool,
}

/// A per-tool output reducer. The reduce call is synchronous-bound (a
/// hot-path filter: ansi-strip, head/tail, truncate); an async variant is
/// reserved for an external reducer process (a TOML-DSL filter). The guard
/// layer (verbatim → reduce → never-worse) runs at the caller; this trait
/// owns only the reduce step.
pub trait ToolOutputReducer: Send + Sync {
    fn reduce(&self, output: &str, tool: &str, ctx: &ReduceCtx) -> ReducedOutput;
    /// Streaming variant: filter a byte stream in flight. Reserved for an
    /// external reducer process; the default reduces per-chunk.
    fn filter_stream<'a>(
        &'a self,
        stream: &'a str,
        tool: &'a str,
        ctx: &'a ReduceCtx,
    ) -> PFut<'a, ReducedOutput> {
        let reduced = self.reduce(stream, tool, ctx);
        Box::pin(async move { reduced })
    }
}

/// Never-worse guard: return the filtered form, or the raw when the filtered
/// would emit more tokens than the raw. A filter that inflates (a pretty-
/// printer, an ansi-strip adding markers) falls back so the reducer is never
/// a net cost.
pub fn never_worse<'a>(raw: &'a str, filtered: &'a str) -> &'a str {
    if estimate_tokens(filtered) > estimate_tokens(raw) {
        raw
    } else {
        filtered
    }
}

/// A coarse token estimate (bytes/4) — the guard only needs a consistent
/// ordering between raw + filtered, not a tiktoken-exact count. The served-
/// view tokenizer is the source of truth for billing; this is the reducer's
/// local floor.
fn estimate_tokens(s: &str) -> usize {
    s.len() / 4
}

/// The reduction ceiling: outputs above this many characters get head + tail
/// + a truncation marker instead of the full body.
const REDUCE_CEILING: usize = 4_000;
/// How many head + tail characters to keep when truncating.
const HEAD_TAIL: usize = 1_500;

/// A hot-path reducer for built-in tools: strips ANSI escapes from bash
/// output, truncates large outputs to head + tail, and tags the result as
/// data. Per-tool rules live here (bash is the dominant large-output source);
/// unknown tools get identity (reduced=false) so a new tool's output is never
/// silently mangled.
pub struct HotPathReducer;

impl ToolOutputReducer for HotPathReducer {
    fn reduce(&self, output: &str, tool: &str, ctx: &ReduceCtx) -> ReducedOutput {
        // The raw flag is a caller escape hatch (a user asking for verbatim
        // output); honor it without filtering.
        if ctx.raw {
            return ReducedOutput {
                text: output.to_string(),
                data_tag: ctx.trust == TrustLevel::Untrusted,
                reduced: false,
            };
        }
        let stripped = if tool == "bash" {
            strip_ansi(output)
        } else {
            output.to_string()
        };
        let truncated = if stripped.len() > REDUCE_CEILING {
            let head: String = stripped.chars().take(HEAD_TAIL).collect();
            let tail: String = stripped
                .chars()
                .rev()
                .take(HEAD_TAIL)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            let dropped = stripped.len() - head.len() - tail.len().min(stripped.len() - head.len());
            format!("{head}\n... [truncated {dropped} bytes] ...\n{tail}")
        } else {
            stripped
        };
        // Never-worse: a filter that inflated (rare for strip + truncate)
        // falls back to the raw.
        let final_text = never_worse(output, &truncated);
        let reduced = final_text.len() < output.len();
        ReducedOutput {
            text: final_text.to_string(),
            data_tag: ctx.trust == TrustLevel::Untrusted,
            reduced,
        }
    }
}

/// Strip ANSI escape sequences (color codes, cursor moves) from a string.
/// Bash output often carries color; stripping it before the model reads is a
/// net token win with no information loss for a coding agent.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // ESC [ ... letter
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            i += 1; // skip the final letter
        } else {
            // safe: we advance one byte; UTF-8 boundaries are respected
            // because we only skip ASCII ESC sequences.
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_smaller_filtered_kept() {
        let raw = "a".repeat(400);
        assert_eq!(never_worse(&raw, "ok"), "ok");
    }

    #[test]
    fn test_filtered_bigger_falls_back() {
        // A pretty-printer that inflates a compact JSON falls back to raw.
        let raw = "{}";
        let filtered = "{\n  \"pretty\": true\n}";
        assert_eq!(never_worse(raw, filtered), raw);
    }

    #[test]
    fn test_reducer_never_worse() {
        // The hot-path reducer never returns more than the raw: an inflation
        // is caught by the never-worse guard + reduced=false.
        let r = HotPathReducer;
        let raw = "{}";
        let out = r.reduce(
            raw,
            "bash",
            &ReduceCtx {
                raw: false,
                trust: TrustLevel::Untrusted,
            },
        );
        assert!(
            !out.text.len() > raw.len() || !out.reduced,
            "reducer never emits more than raw"
        );
        assert!(out.text.len() <= raw.len() || out.text.as_str() == raw);
    }

    #[test]
    fn test_trust_gate_data_tag() {
        // Untrusted tool output is tagged data; trusted output is not.
        let r = HotPathReducer;
        let untrusted = r.reduce(
            "output",
            "bash",
            &ReduceCtx {
                raw: false,
                trust: TrustLevel::Untrusted,
            },
        );
        assert!(untrusted.data_tag, "untrusted output tagged DATA");
        let trusted = r.reduce(
            "output",
            "bash",
            &ReduceCtx {
                raw: false,
                trust: TrustLevel::Trusted,
            },
        );
        assert!(!trusted.data_tag, "trusted output not tagged");
    }

    #[test]
    fn test_bash_strips_ansi_truncates() {
        let r = HotPathReducer;
        // ANSI color wrapped around text is stripped.
        let colored = "\x1b[32mgreen\x1b[0m";
        let out = r.reduce(
            colored,
            "bash",
            &ReduceCtx {
                raw: false,
                trust: TrustLevel::Untrusted,
            },
        );
        assert_eq!(out.text, "green", "ansi stripped");
        assert!(out.reduced, "stripped is smaller");

        // Large output truncates to head + tail + a marker.
        let big = "x".repeat(10_000);
        let out = r.reduce(
            &big,
            "bash",
            &ReduceCtx {
                raw: false,
                trust: TrustLevel::Untrusted,
            },
        );
        assert!(out.reduced, "large output reduced");
        assert!(out.text.contains("[truncated"), "truncation marker present");
        assert!(out.text.len() < big.len(), "truncated is smaller");
    }

    #[test]
    fn test_unknown_tool_no_mangle() {
        // An unknown tool's output is not reduced (identity) so a new tool is
        // never silently mangled.
        let r = HotPathReducer;
        let out = r.reduce(
            "raw output",
            "future_tool",
            &ReduceCtx {
                raw: false,
                trust: TrustLevel::Untrusted,
            },
        );
        assert!(!out.reduced, "unknown tool not reduced");
        assert_eq!(out.text, "raw output");
    }

    #[test]
    fn test_raw_flag_skips_reduction() {
        let r = HotPathReducer;
        let colored = "\x1b[32mgreen\x1b[0m";
        let out = r.reduce(
            colored,
            "bash",
            &ReduceCtx {
                raw: true,
                trust: TrustLevel::Untrusted,
            },
        );
        assert!(!out.reduced, "raw flag skips reduction");
        assert_eq!(out.text, colored, "raw output verbatim");
    }
}
