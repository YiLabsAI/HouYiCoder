//! The result shape per output mode, the offset and head-limit page, and the
//! JSON the model receives.
//!
//! Pagination fields appear only when part of the result was dropped, so the
//! model can tell a complete answer from a page and ask for the next one.

use serde_json::{Value, json};

use super::input::{GrepOptions, OutputMode};

/// Collected grep results before JSON serialization.
pub(super) struct GrepOutput {
    pub(super) mode: OutputMode,
    pub(super) filenames: Vec<String>,
    pub(super) content: String,
    pub(super) num_files: usize,
    pub(super) num_lines: usize,
    pub(super) num_matches: usize,
    pub(super) truncated: bool,
    pub(super) applied_limit: Option<usize>,
    pub(super) applied_offset: Option<usize>,
}

impl GrepOutput {
    pub(super) fn to_json(&self) -> Value {
        let mode_str = match self.mode {
            OutputMode::Content => "content",
            OutputMode::FilesWithMatches => "files_with_matches",
            OutputMode::Count => "count",
        };
        let mut v = json!({
            "mode": mode_str,
            "filenames": self.filenames,
            "num_files": self.num_files,
            "content": self.content,
        });
        if self.truncated {
            v["truncated"] = json!(true);
        }
        if let Some(limit) = self.applied_limit {
            v["applied_limit"] = json!(limit);
        }
        if let Some(offset) = self.applied_offset {
            v["applied_offset"] = json!(offset);
        }
        match self.mode {
            OutputMode::Content => {
                v["num_lines"] = json!(self.num_lines);
            }
            OutputMode::Count => {
                v["num_matches"] = json!(self.num_matches);
            }
            _ => {}
        }
        v
    }
}

/// Assemble the final GrepOutput, applying offset + head_limit. The
/// applied_limit/applied_offset fields are set only when truncation
/// occurred, so the model knows it can paginate.
pub(super) fn build_output(
    opts: &GrepOptions,
    content_lines: Vec<String>,
    match_files: Vec<String>,
    count_lines: Vec<String>,
    truncated: bool,
) -> GrepOutput {
    let offset = opts.offset;
    let limit = opts.head_limit;
    match opts.output_mode {
        OutputMode::Content => {
            let (lines, was_truncated) = apply_limit(content_lines, limit, offset);
            let applied_limit = if was_truncated || truncated {
                Some(limit)
            } else {
                None
            };
            let applied_offset = if offset > 0 { Some(offset) } else { None };
            GrepOutput {
                mode: OutputMode::Content,
                filenames: Vec::new(),
                content: lines.join("\n"),
                num_files: 0,
                num_lines: lines.len(),
                num_matches: 0,
                truncated: was_truncated || truncated || applied_limit.is_some(),
                applied_limit,
                applied_offset,
            }
        }
        OutputMode::FilesWithMatches => {
            let (files, was_truncated) = apply_limit(match_files, limit, offset);
            let applied_limit = if was_truncated || truncated {
                Some(limit)
            } else {
                None
            };
            let applied_offset = if offset > 0 { Some(offset) } else { None };
            let num_files = files.len();
            GrepOutput {
                mode: OutputMode::FilesWithMatches,
                filenames: files,
                content: String::new(),
                num_files,
                num_lines: 0,
                num_matches: 0,
                truncated: was_truncated || truncated,
                applied_limit,
                applied_offset,
            }
        }
        OutputMode::Count => {
            let (lines, was_truncated) = apply_limit(count_lines, limit, offset);
            let applied_limit = if was_truncated || truncated {
                Some(limit)
            } else {
                None
            };
            let applied_offset = if offset > 0 { Some(offset) } else { None };
            // Sum counts from the limited lines (post-apply_limit) so
            // num_matches matches what the model sees. Lines: "path:count".
            let num_matches: usize = lines
                .iter()
                .filter_map(|l| l.rsplit(':').next().and_then(|c| c.parse::<usize>().ok()))
                .sum();
            GrepOutput {
                mode: OutputMode::Count,
                filenames: Vec::new(),
                content: lines.join("\n"),
                num_files: lines.len(),
                num_lines: 0,
                num_matches,
                truncated: was_truncated || truncated,
                applied_limit,
                applied_offset,
            }
        }
    }
}

/// Apply offset and head_limit (0 = unlimited). Returns sliced items + a
/// truncated flag (true only when items were dropped).
pub(super) fn apply_limit<T: Clone>(items: Vec<T>, limit: usize, offset: usize) -> (Vec<T>, bool) {
    let total = items.len();
    if limit == 0 {
        let skipped: Vec<T> = items.into_iter().skip(offset).collect();
        let was_truncated = offset > 0 && total > offset + skipped.len();
        return (skipped, was_truncated);
    }
    let was_truncated = total.saturating_sub(offset) > limit;
    let sliced: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    (sliced, was_truncated)
}
