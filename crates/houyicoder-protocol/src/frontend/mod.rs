//! Frontend protocol — message set + slash commands. Stub types; NDJSON
//! framing + capability enforcement to come. See the command spec and the
//! interface spec docs for the contract.

#![allow(dead_code)] // wire types serialized for cross-crate use; locally unused

use serde::{Deserialize, Serialize};

pub mod compact;
pub mod context;
pub mod debug;
pub mod event_kind;
pub mod evidence;
pub mod hooks;
pub mod memory;
pub mod model;
pub mod permission;
pub mod run;
pub mod session_update;
pub mod skills;
pub mod status;
pub mod tools;
pub mod trajectory;
pub mod trust;
pub mod verdict;
pub use compact::*;
pub use context::*;
pub use event_kind::{FrontendEvent, FrontendEventKind};
pub use evidence::*;
pub use memory::*;
pub use permission::*;
pub use run::*;
pub use trajectory::TrajectoryEntry;
pub use trust::*;
pub use verdict::Verdict;

/// A wire session id, carried by run verbs (MessageSend, RunCancel) so a
/// multi-session host can route a request to the session it drives. A
/// transparent newtype over String so the wire shape is an unadorned string
/// and a typed surface catches a bare String leaking into a session-routed
/// call.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Slash commands. UI palette; each maps to a frontend protocol method or is
/// handled locally (see the command spec). Placeholder until the daemon wires
/// real responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    // session and auth
    Login,
    Console,
    Clear,
    Rewind,
    Resume,
    Replay,
    Exit,
    // spec-driven main flow (guided chain)
    Init,
    Plan,
    Implement,
    Review,
    Verify,
    // capability panes
    Spec,
    Graph,
    Diff,
    Agents,
    Memory,
    Model,
    Sandbox,
    /// Open the interactive permission-rule manager (tabs: Allow/Ask/Deny
    /// /Recent; add / remove / search). TUI-local; no server round-trip to
    /// open.
    Permission,
    // utility
    Context,
    Status,
    Trajectory,
    Tools,
    /// /skills: show loaded skills (name, description, source, body
    /// token estimate). Renders from the context cache's Skills
    /// section; sends a ContextQuery if no cache yet.
    Skills,
    /// /export [path]: write the session's durable trajectory + tool stats +
    /// usage + checkpoints + errors to a JSON file (the self-evolution ExPeL
    /// data source). TUI-local — no server round-trip. Argless writes to a
    /// default filename in the cwd; an explicit path overrides it.
    Export,
    ReleaseNotes,
    Help,
    Worktree,
    Compact,
    /// Undo the most recent recoverable destructive operation (a CoW snapshot
    /// restore or a per-file before-image).
    Undo,
    // TUI-local commands registered in the palette so they are discoverable
    // (previously lived only in the string dispatcher and were invisible
    // when the user typed /). Arg-bearing forms still parse in the TUI's
    // string dispatcher; these variants drive palette display + the
    // argless select path. The /artifact + /artifact-save document
    // commands stay string-only on purpose — arg-bearing forms the palette's
    // argless select path cannot carry.
    /// /search <query>: takes a query argument (palette inserts the trailing
    /// space and waits).
    Search,
    /// /hooks: list registered hooks (read-only visibility — which hook
    /// events are wired, their source). Read-only (not an interactive
    /// config menu).
    Hooks,
    /// /debug [path | off]: toggle runtime debug logging without restarting.
    /// Registered in the palette (discoverable when the user types /) so the
    /// runtime-logging surface is visible like /status and /context, not a
    /// hidden string-only command. Arg-bearing: [/path] turns logging on at a
    /// path, "off" turns it off.
    Debug,
}

/// One row in the slash-command table: the enum variant, its palette /name,
/// and a one-line help string. The table is the single source of truth; ALL,
/// name(), help(), and parse() all read from it. Adding a command is one
/// entry here instead of four parallel match arms.
struct CommandDescriptor {
    variant: SlashCommand,
    name: &'static str,
    help: &'static str,
}

/// The command table in palette order. Add a command by appending one row;
/// ALL, name(), help(), and parse() derive from this automatically.
const COMMANDS: &[CommandDescriptor] = &[
    CommandDescriptor {
        variant: SlashCommand::Login,
        name: "/login",
        help: "sign in (SSO / API key / local mode)",
    },
    CommandDescriptor {
        variant: SlashCommand::Console,
        name: "/console",
        help: "open the enterprise console",
    },
    CommandDescriptor {
        variant: SlashCommand::Clear,
        name: "/clear",
        help: "start fresh; previous session archived (resumable with /resume)",
    },
    CommandDescriptor {
        variant: SlashCommand::Rewind,
        name: "/rewind",
        help: "rewind to an earlier stage (no args: one step; named: jump to that stage)",
    },
    CommandDescriptor {
        variant: SlashCommand::Resume,
        name: "/resume",
        help: "resume an archived session (partial reload)",
    },
    CommandDescriptor {
        variant: SlashCommand::Replay,
        name: "/replay",
        help: "replay a session (reproducible)",
    },
    CommandDescriptor {
        variant: SlashCommand::Exit,
        name: "/exit",
        help: "exit hicoder",
    },
    CommandDescriptor {
        variant: SlashCommand::Init,
        name: "/init",
        help: "create project spec and memory",
    },
    CommandDescriptor {
        variant: SlashCommand::Plan,
        name: "/plan",
        help: "draft a plan (steps)",
    },
    CommandDescriptor {
        variant: SlashCommand::Implement,
        name: "/implement",
        help: "implement (work log + per-hunk diff approval)",
    },
    CommandDescriptor {
        variant: SlashCommand::Review,
        name: "/review",
        help: "run multi-agent adversarial review",
    },
    CommandDescriptor {
        variant: SlashCommand::Verify,
        name: "/verify",
        help: "verify (Z3 / tests / eval)",
    },
    CommandDescriptor {
        variant: SlashCommand::Spec,
        name: "/spec",
        help: "draft or view the current spec",
    },
    CommandDescriptor {
        variant: SlashCommand::Graph,
        name: "/graph",
        help: "code graph query (impact_set ...)",
    },
    CommandDescriptor {
        variant: SlashCommand::Diff,
        name: "/diff",
        help: "view / apply diff",
    },
    CommandDescriptor {
        variant: SlashCommand::Agents,
        name: "/agents",
        help: "show A2A fleet status",
    },
    CommandDescriptor {
        variant: SlashCommand::Memory,
        name: "/memory",
        help: "recall / browse structured memory",
    },
    CommandDescriptor {
        variant: SlashCommand::Model,
        name: "/model",
        help: "switch / inspect model",
    },
    CommandDescriptor {
        variant: SlashCommand::Sandbox,
        name: "/sandbox",
        help: "sandbox / capability status",
    },
    CommandDescriptor {
        variant: SlashCommand::Permission,
        name: "/permissions",
        help: "manage permission rules + ask-before-git toggle",
    },
    CommandDescriptor {
        variant: SlashCommand::Context,
        name: "/context",
        help: "visualize token budget and context usage",
    },
    CommandDescriptor {
        variant: SlashCommand::Status,
        name: "/status",
        help: "show version / model / sandbox / connectivity",
    },
    CommandDescriptor {
        variant: SlashCommand::Trajectory,
        name: "/trajectory",
        help: "replay the event log: kind / ts / hash-chain",
    },
    CommandDescriptor {
        variant: SlashCommand::Tools,
        name: "/tools",
        help: "list registered tools (capability discoverability)",
    },
    CommandDescriptor {
        variant: SlashCommand::Skills,
        name: "/skills",
        help: "show loaded skills (name, description, source, token cost)",
    },
    CommandDescriptor {
        variant: SlashCommand::Export,
        name: "/export",
        help: "write the session trajectory to a JSON file (ExPeL data source)",
    },
    CommandDescriptor {
        variant: SlashCommand::ReleaseNotes,
        name: "/release-notes",
        help: "view what's new",
    },
    CommandDescriptor {
        variant: SlashCommand::Help,
        name: "/help",
        help: "show help",
    },
    CommandDescriptor {
        variant: SlashCommand::Worktree,
        name: "/worktrees",
        help: "worktrees management",
    },
    CommandDescriptor {
        variant: SlashCommand::Compact,
        name: "/compact",
        help: "free up context by summarizing the conversation so far",
    },
    CommandDescriptor {
        variant: SlashCommand::Undo,
        name: "/undo",
        help: "undo the most recent destructive operation",
    },
    CommandDescriptor {
        variant: SlashCommand::Search,
        name: "/search",
        help: "search the transcript",
    },
    CommandDescriptor {
        variant: SlashCommand::Hooks,
        name: "/hooks",
        help: "list registered hooks (read-only)",
    },
    CommandDescriptor {
        variant: SlashCommand::Debug,
        name: "/debug",
        help: "toggle runtime debug logging [/path | off]",
    },
];

/// Build the ALL array from the command table at compile time so ALL and
/// COMMANDS cannot drift apart. Const-eval reads the table by index.
const fn all_variants() -> [SlashCommand; 34] {
    let mut out = [SlashCommand::Login; 34];
    let mut i = 0;
    while i < 34 {
        out[i] = COMMANDS[i].variant;
        i += 1;
    }
    out
}

impl SlashCommand {
    /// All commands in palette order. Derived from the COMMANDS table at
    /// compile time, so the array and the table stay in sync.
    pub const ALL: [SlashCommand; 34] = all_variants();

    /// Parse a /foo input line. None if not a recognized command. Linear
    /// search over the COMMANDS table by name.
    pub fn parse(input: &str) -> Option<Self> {
        let trimmed = input.trim();
        COMMANDS
            .iter()
            .find(|d| d.name == trimmed)
            .map(|d| d.variant)
    }

    /// The /name form shown in the palette. Read from the COMMANDS table.
    pub fn name(&self) -> &'static str {
        COMMANDS
            .iter()
            .find(|d| d.variant == *self)
            .expect("COMMANDS table covers every SlashCommand variant")
            .name
    }

    /// One-line description (palette help column). Read from the COMMANDS
    /// table.
    pub fn help(&self) -> &'static str {
        COMMANDS
            .iter()
            .find(|d| d.variant == *self)
            .expect("COMMANDS table covers every SlashCommand variant")
            .help
    }

    /// Whether selecting this command from the palette should keep the popup
    /// open + wait for the user to type an argument (true) versus auto-run on
    /// select (false). Only commands whose argless form is a dead-end keep the
    /// popup open; commands with a valid argless form (/resume opens the
    /// picker, /export writes a default file, /debug toggles logging) run
    /// argless on select. The user can still type an argument by typing
    /// "name " + arg in the popup (the raw-submit path handles it), guided by
    /// the arg_hint shown after the space — houyi keeps the hint visible
    /// after a space, so the user is not left blind-typing.
    pub fn takes_arg(&self) -> bool {
        matches!(self, SlashCommand::Search)
    }

    /// The argument usage hint shown in the palette once the query is a command
    /// name + a trailing space (e.g. "resume "): the format the trailing
    /// argument should take. Empty for commands that take no argument. The
    /// popup stays open after the space and renders this dim line so the user
    /// sees what to type instead of guessing — the hint-after-space design.
    pub fn arg_hint(&self) -> &'static str {
        match self {
            SlashCommand::Search => "<KEYWORD>",
            SlashCommand::Resume => "<file.json | session name | sid>",
            SlashCommand::Export => "[path]",
            SlashCommand::Debug => "[path | off]",
            _ => "",
        }
    }

    /// Whether a skill name collides with a builtin slash command. Skills
    /// whose name matches a builtin are rejected at registration so they
    /// cannot shadow the builtin — a project skill named "compact" must not
    /// hijack /compact. The leading slash is stripped so a skill name
    /// "compact" matches the command "/compact".
    pub fn is_reserved_skill_name(name: &str) -> bool {
        all_variants()
            .iter()
            .any(|v| v.name().trim_start_matches('/') == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The COMMANDS table must list every variant exactly once, in ALL order,
    /// so name()/help()/parse() never panic and ALL stays in sync.
    #[test]
    fn test_commands_table_covers() {
        assert_eq!(COMMANDS.len(), SlashCommand::ALL.len());
        let from_table: Vec<SlashCommand> = COMMANDS.iter().map(|d| d.variant).collect();
        assert_eq!(from_table, SlashCommand::ALL.to_vec());
        for v in SlashCommand::ALL {
            assert!(!v.name().is_empty());
            assert!(!v.help().is_empty());
        }
    }

    #[test]
    fn test_parse_round_trips_names() {
        for v in SlashCommand::ALL {
            assert_eq!(SlashCommand::parse(v.name()), Some(v));
        }
    }

    #[test]
    fn test_parse_rejects_unknown() {
        assert_eq!(SlashCommand::parse("/nope"), None);
        assert_eq!(SlashCommand::parse("login"), None);
    }

    /// /debug is registered (parseable + has a name/help) so it is discoverable
    /// in the palette like /status and /context, not a hidden string command.
    #[test]
    fn test_debug_is_registered() {
        assert_eq!(SlashCommand::parse("/debug"), Some(SlashCommand::Debug));
        assert!(SlashCommand::Debug.name().contains("debug"));
        assert!(!SlashCommand::Debug.help().is_empty());
    }

    /// Arg-capable commands advertise a non-empty hint (so the palette can show it
    /// after a space); arg-incapable ones do not. Every takes_arg command must
    /// have a hint, but a hint does not imply takes_arg (/resume /export /debug
    /// take an optional arg but run argless on select).
    #[test]
    fn test_arg_hint_arg_capable() {
        for c in SlashCommand::ALL {
            if c.takes_arg() {
                assert!(
                    !c.arg_hint().is_empty(),
                    "{} takes an arg but has no hint",
                    c.name()
                );
            }
        }
        // The optional-arg commands advertise a hint even though they are not
        // takes_arg (their argless form runs on select; the hint guides the
        // spaced form).
        for name in ["resume", "export", "debug"] {
            let cmd = SlashCommand::parse(&format!("/{name}"));
            let cmd = cmd.unwrap_or_else(|| panic!("{name} parsed"));
            assert!(
                !cmd.arg_hint().is_empty(),
                "{name} should advertise an arg hint"
            );
        }
    }

    #[test]
    fn test_all_variants_runs_runtime() {
        // The const fn that builds ALL is evaluated at compile time for the
        // const; call it at runtime so its body is covered too.
        let v = all_variants();
        assert_eq!(v, SlashCommand::ALL);
        assert_eq!(v.len(), SlashCommand::ALL.len());
    }
}

/// Frontend -> daemon requests (NDJSON, capability-gated). One verb per
/// slash command that needs the daemon (local commands like /help are not
/// here). See the command spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FrontendRequest {
    // session and auth
    Login {
        mode: LoginMode,
    },
    Console,
    Archive {
        session_id: String,
    },
    /// Reset the runner's cumulative usage tally + audit trajectory for the
    /// session (the /clear command). The host clears its local view; this
    /// zeros the server-side counters so /status reflects the fresh session.
    SessionReset {
        session_id: SessionId,
    },
    SessionLoad {
        id: String,
    },
    SessionCancel {
        id: String,
    },
    Replay {
        id: String,
    },
    // spec-driven flow
    Init {
        task: String,
    },
    Plan {
        spec_id: String,
    },
    Implement {
        plan_id: String,
    },
    Review {
        target: String,
    },
    Verify {
        target: String,
    },
    // capability
    Spec {
        spec_id: String,
    },
    GraphQuery {
        query: String,
    },
    DiffApply {
        path: String,
        patch: String,
    },
    DiffReject {
        path: String,
    },
    Agents,
    /// Fetch a child agent's transcript on demand (expanding a Subagent
    /// fold-group in the parent flow). The child session id is the agentId
    /// the sync spawn path returned; the reply is a one-shot snapshot of the
    /// child's turn events projected to the same session/update + acpx frame
    /// stream the parent accumulates. A sync child is terminal by the time
    /// the fold-group renders, so this is a full snapshot, not a live stream.
    ChildTranscript {
        child_sid: SessionId,
    },
    MemoryRecall {
        query: String,
    },
    ModelInfo,
    /// Switch the runner's active model (the /model pane select). The host
    /// resolves a Default sentinel, applies the model id, and persists the
    /// pick; the reply carries the applied ModelApplied so the status bar
    /// renders what is actually being sent. model None means Default; effort
    /// None means the user left effort on auto; effort_toggled records whether
    /// the user touched effort in the picker, which the host needs to apply the
    /// persistence rule (effort equal to the model default is not persisted).
    ModelSet {
        model: Option<String>,
        effort: Option<crate::llm::EffortLevel>,
        effort_toggled: bool,
    },
    /// Rename the current session: persist the display name to the session
    /// sidecar (name + name_source=User). An empty name clears back to Auto
    /// (name=None; the display derives the first-prompt slug at render). The
    /// server replies with the applied StatusSnapshot so the host re-renders
    /// /status + the picker reflects the new name on the next list.
    /// Houyi derives Auto names deterministically from the first prompt
    /// (no LLM name-generation round-trip).
    RenameSession {
        session_id: SessionId,
        name: String,
    },
    SandboxStatus,
    // utility
    MessageSend {
        session_id: SessionId,
        content: Vec<crate::frontend::run::ContentBlock>,
    },
    /// Abort the in-flight run. The service cancels the loop, flushes partial
    /// text, and the next outcome returns Interrupted. Fire-and-forget; the
    /// outcome arrives as the response to the original run. Carries the
    /// session id so a multi-session host routes the cancel to the right run.
    RunCancel {
        session_id: SessionId,
        reason: String,
    },
    ToolList,
    Hooks,
    Skills,
    ToolCall {
        tool: String,
        args: String,
    },
    Context,
    Status,
    Trajectory,
    /// List every stored memory as a frontmatter-only summary (no body).
    MemoryList,
    /// Fetch the full body of one memory by key.
    MemoryShow {
        key: String,
    },
    /// Read both memory toggles (auto-memory + auto-dream) so the /memory pane
    /// renders the on/off rows from the persisted + in-memory state.
    MemoryToggleState,
    /// Flip one memory toggle and persist the new pair. The response carries
    /// the full snapshot so the pane re-renders both rows from one round-trip.
    MemoryToggle {
        which: crate::frontend::memory::MemoryToggleWhich,
    },
    /// Forget one memory by key (the /memory pane d action or the
    /// /memory forget command). The server deletes the topic + replies with
    /// the refreshed MemoryList so the pane narrows without a second request.
    /// scope is the row's storage scope (user / project / auto) so the
    /// delete routes to the matching root, not just the auto copy.
    MemoryForget {
        key: String,
        scope: String,
    },
    Worktree,
    Compact,
    Metrics,
    // permission mode and rule set (the TUI renders /mode and /rules without
    // importing the permission crate; the service projects the engine gate
    // state to the wire form at the boundary).
    PermissionMode,
    PermissionRules,
    PermissionCycleMode,
    PermissionAddRule {
        rule: crate::frontend::permission::PermissionRule,
    },
    PermissionRemoveRule {
        index: usize,
    },
    /// Add a directory the sandboxed agent may touch beyond the workspace
    /// root. The server canonicalizes + validates the path is a directory,
    /// extends the kernel fence's allow-back, and responds with the updated
    /// directory list so the Workspace tab stays in sync without a poll. A
    /// path that is not a directory surfaces as a ResponsePayload::Error.
    PermissionAddWorkingDir {
        path: String,
    },
    /// Remove a previously-added working directory. No-op when the path was
    /// never added (or was deleted since); the response carries the updated
    /// list either way.
    PermissionRemoveWorkingDir {
        path: String,
    },
    /// Query or set the git-confirm checkpoint toggle (git commit/rebase/reset/tag Ask
    /// before running). enabled=None queries the current state; Some sets it.
    /// The response always carries the resulting state so the /permission view
    /// stays in sync without a separate poll.
    PermissionAskBeforeGit {
        enabled: Option<bool>,
    },
    /// Undo the most recent recoverable destructive operation (a CoW snapshot
    /// or a per-file before-image). The server pops the undo stack and
    /// restores the workspace; the reply carries the entry description so
    /// the host can surface what was undone.
    Undo,
    // evidence-id coupling (evidence and audit prototype): click-jump and sign-off / replay verbs.
    // All stub.
    SpecClauseJump {
        clause_id: String,
    },
    FindingJump {
        finding_id: String,
    },
    TestJump {
        test_id: String,
    },
    SignOff {
        finding_id: String,
        verdict: Verdict,
    },
    ReplayToPoint {
        event_id: String,
    },
    /// Toggle the process-wide diagnostic log level (the /debug command).
    /// The level applies to every crate that uses the tracing macros, so a
    /// single request turns the engine, the sandbox and the permission gate
    /// on or off together. The response carries the resulting state (enabled
    /// + path) so the host can surface where the file is without guessing.
    DebugSet {
        level: crate::frontend::debug::DebugLevel,
    },
}

/// Login mode (enterprise SSO, API key, or local/offline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginMode {
    Sso,
    ApiKey,
    Local,
}
