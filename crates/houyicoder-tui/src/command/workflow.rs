//! The stage chain and the artifact document commands: entering a stage,
//! rewinding the chain, and opening or saving the artifact being reviewed.

use crate::artifact::ArtifactSession;
use crate::state::{App, Pane, Stage, pane_for_stage};

impl App {
    /// Enter a guided stage: push the old stage onto the history, set the new
    /// stage + pane, update the spec strip step, and log the transition.
    pub(crate) fn enter_stage(&mut self, stage: Stage, pane: Pane, msg: &str) {
        self.set_stage(stage);
        self.ui.navigation.pane = pane;
        self.system_line(msg.to_string());
    }

    /// /rewind: pop the last stage transition, land on the matching pane, and
    /// un-approve the artifact of the restored stage so it can be re-drafted.
    pub(crate) fn rewind(&mut self) {
        match self.rewind_stage() {
            Some(prev) => {
                self.ui.navigation.pane = pane_for_stage(prev);
                self.unapprove_artifact(prev);
                self.system_line(format!(
                    "rewound to {} (artifact un-approved)",
                    prev.label()
                ));
            }
            None => self.system_line("rewind: no earlier stage to return to"),
        }
    }

    /// /rewind <stage_name>: rewind to a named stage (spec, plan, implement,
    /// review, verify). Pops history until the target is on top, un-approves
    /// its artifact, and lands on the matching pane. An unknown name reports
    /// an error and returns (no fallback to a one-step rewind — a typo must
    /// not silently rewind). The /memory sub-command (toggle / forget / show)
    /// lives in the memory command module.
    pub(crate) fn rewind_targeted(&mut self, name: &str) {
        if name.is_empty() {
            self.rewind();
            return;
        }
        let Some(target) = parse_stage_name(name) else {
            self.system_line(format!("rewind: unknown stage {name}"));
            return;
        };
        // Pop history until the current stage matches the target (or history
        // runs out), then ensure the target is active.
        while self.workflow.stage != target {
            if self.rewind_stage().is_none() {
                break;
            }
        }
        if self.workflow.stage != target {
            self.set_stage(target);
        }
        self.ui.navigation.pane = pane_for_stage(target);
        self.unapprove_artifact(target);
        self.system_line(format!(
            "rewound to {} (artifact un-approved)",
            target.label()
        ));
    }

    /// Un-approve the artifact associated with a stage so returning to it
    /// forces a fresh approval. Spec and plan have explicit approved flags;
    /// later stages have no single artifact flag so their work is reset by the
    /// chain reset path instead.
    fn unapprove_artifact(&mut self, stage: Stage) {
        if stage == Stage::Design {
            self.workflow.spec_artifact.approved = false;
            self.workflow.plan_artifact.approved = false;
        }
    }

    /// /artifact [path] and /artifact-save [path]: the document commands.
    /// /artifact opens the inline-review pane, loading the given path or the
    /// canned stub when none is given; /artifact-save writes the edited
    /// document back. Both are TUI-local, and neither is in the protocol
    /// palette, so they only arrive here as typed input.
    pub(crate) fn run_artifact_command(&mut self, name: &str) -> bool {
        // /artifact-save [path]: an explicit path wins; with none, the
        // document is written back to the file it was loaded from, and the
        // stub has none to write to. No auto-save: explicit only.
        if name == "artifact-save" || name.starts_with("artifact-save ") {
            if self.ui.navigation.pane != Pane::Artifact {
                self.system_line("artifact-save: open an artifact first (/artifact <path>)");
                return true;
            }
            let arg = name.strip_prefix("artifact-save").unwrap_or("").trim();
            let path = if arg.is_empty() {
                match self.workflow.artifact.file_path() {
                    Some(loaded) => loaded.to_string(),
                    None => {
                        self.system_line("artifact-save: pass a path; the stub has no file");
                        return true;
                    }
                }
            } else {
                arg.to_string()
            };
            let lines = self.workflow.artifact.current_lines().len();
            match self.workflow.artifact.save(&path) {
                Ok(()) => self.system_line(format!("artifact: saved {path} ({lines} lines)")),
                Err(_) => self.system_line(format!("artifact: could not write {path}")),
            }
            return true;
        }
        // /artifact [path]: a path loads that document; no path opens the
        // canned stub, which is not a file, so the pane names it as a stub
        // rather than a path nothing backs. Always opens in Working mode:
        // artifact editing needs the input box, and Focus mode hides it
        // (which would dead-end the c/o/d keys).
        if name == "artifact" || name.starts_with("artifact ") {
            let arg = name.strip_prefix("artifact").unwrap_or("").trim();
            if arg.is_empty() {
                self.workflow.artifact = ArtifactSession::stub();
                self.ui.navigation.pane = Pane::Artifact;
                self.system_line("artifact: canned stub (pass a path to open a file)");
            } else {
                match ArtifactSession::load(arg) {
                    Ok(session) => {
                        self.workflow.artifact = session;
                        self.ui.navigation.pane = Pane::Artifact;
                        self.system_line(format!(
                            "artifact: opened {} (c=replace o=insert d=delete i=nl; a/r=review)",
                            self.workflow.artifact.path()
                        ));
                    }
                    Err(_) => {
                        self.ui.navigation.pane = Pane::Artifact;
                        self.system_line(format!(
                            "artifact: could not read {arg}, showing canned stub"
                        ));
                    }
                }
            }
            self.fold_to_working();
            return true;
        }
        false
    }
}

/// Parse a stage name for /rewind <stage_name>. Accepts the short labels and
/// common synonyms. Returns None for an unrecognized name.
fn parse_stage_name(name: &str) -> Option<Stage> {
    match name.trim().to_ascii_lowercase().as_str() {
        "idle" => Some(Stage::Idle),
        "design" | "spec" | "plan" => Some(Stage::Design),
        "implement" | "implementing" => Some(Stage::Implementing),
        "verify" | "review" | "verifying" | "reviewing" => Some(Stage::Verify),
        "done" => Some(Stage::Done),
        _ => None,
    }
}
