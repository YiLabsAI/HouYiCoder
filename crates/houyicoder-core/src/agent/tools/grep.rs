//! File content search tool: regex search across the workspace.
//! Built on walkdir + the regex crate, in-process. Read-only and
//! concurrency-safe. Output paths relativized to the workspace root; VCS +
//! build-output dirs excluded. Three modes: content, files_with_matches,
//! count. A head_limit caps output; a large result is later isolated into a
//! content-addressed block rather than truncated away.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};

use input::GrepOptions;

mod filters;
mod input;
mod output;
mod search;
mod subprocess;

#[cfg(test)]
mod tests;

/// A content search tool backed by the sandbox workspace.
pub struct GrepTool {
    session: Arc<dyn SandboxSession>,
}

impl GrepTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self { session }
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with regex. \
         Input: {pattern, path?, glob?, output_mode?, -A?, -B?, -C?, context?, \
         head_limit?, offset?, multiline?, -i?, -n?, type?}. \
         output_mode: content | files_with_matches | count (default). \
         head_limit default 250, 0 = unlimited. offset default 0. \
         -n default true (content mode). VCS dirs excluded."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "glob": {"type": "string"},
                "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                "-A": {"type": "number"},
                "-B": {"type": "number"},
                "-C": {"type": "number"},
                "context": {"type": "number"},
                "head_limit": {"type": "number"},
                "offset": {"type": "number"},
                "multiline": {"type": "boolean"},
                "-i": {"type": "boolean"},
                "-n": {"type": "boolean"},
                "type": {"type": "string"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let root = self.session.workspace_root().to_path_buf();
        let dirs = self.session.working_dirs();
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            let opts = GrepOptions::from_input(&input)?;
            subprocess::dispatch_grep(root, dirs, opts, cancel).await
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
}
