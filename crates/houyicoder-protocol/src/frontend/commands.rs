//! The slash-command vocabulary: the palette's command set, its table, and
//! the parsing and display every surface reads from it. One table is the
//! source of truth; the derived arrays and lookups all read from it.

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
    /// /tasks: the commands that outlived their tool call. The pane lists
    /// every task with its state, run time, and command, opens one's live
    /// output, and stops a running one. The same pane opens when Down is
    /// pressed on an empty input.
    Tasks,
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
    /// /mcp: manage external tool servers (list, reconnect, toggle).
    Mcp,
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
        help: "Start a fresh session and archive the current one",
    },
    CommandDescriptor {
        variant: SlashCommand::Rewind,
        name: "/rewind",
        help: "open the checkpoints a session can restore (named: jump to that stage)",
    },
    CommandDescriptor {
        variant: SlashCommand::Resume,
        name: "/resume",
        help: "Browse and resume a previous session",
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
        help: "View uncommitted changes and file diffs",
    },
    CommandDescriptor {
        variant: SlashCommand::Agents,
        name: "/agents",
        help: "View running subagents, history, and agent types",
    },
    CommandDescriptor {
        variant: SlashCommand::Tasks,
        name: "/tasks",
        help: "View and manage background commands",
    },
    CommandDescriptor {
        variant: SlashCommand::Memory,
        name: "/memory",
        help: "Browse and manage session and project memories",
    },
    CommandDescriptor {
        variant: SlashCommand::Model,
        name: "/model",
        help: "Switch model, reasoning effort, and speed mode",
    },
    CommandDescriptor {
        variant: SlashCommand::Sandbox,
        name: "/sandbox",
        help: "Inspect sandbox boundaries and recovery checkpoints",
    },
    CommandDescriptor {
        variant: SlashCommand::Permission,
        name: "/permissions",
        help: "Manage tool permission rules and workspace access",
    },
    CommandDescriptor {
        variant: SlashCommand::Context,
        name: "/context",
        help: "Visualize context window usage and token breakdown",
    },
    CommandDescriptor {
        variant: SlashCommand::Status,
        name: "/status",
        help: "Show session status, model, sandbox, and account info",
    },
    CommandDescriptor {
        variant: SlashCommand::Trajectory,
        name: "/trajectory",
        help: "Inspect turn latency, token usage, and tool timeline",
    },
    CommandDescriptor {
        variant: SlashCommand::Tools,
        name: "/tools",
        help: "List available built-in and MCP tools",
    },
    CommandDescriptor {
        variant: SlashCommand::Skills,
        name: "/skills",
        help: "Browse loaded skills, sources, and token costs",
    },
    CommandDescriptor {
        variant: SlashCommand::Export,
        name: "/export",
        help: "Export the session transcript and trajectory to a file",
    },
    CommandDescriptor {
        variant: SlashCommand::ReleaseNotes,
        name: "/release-notes",
        help: "view what's new",
    },
    CommandDescriptor {
        variant: SlashCommand::Help,
        name: "/help",
        help: "Show available commands and keyboard shortcuts",
    },
    CommandDescriptor {
        variant: SlashCommand::Worktree,
        name: "/worktrees",
        help: "List, enter, or remove isolated git worktrees",
    },
    CommandDescriptor {
        variant: SlashCommand::Compact,
        name: "/compact",
        help: "Summarize earlier conversation to free up context",
    },
    CommandDescriptor {
        variant: SlashCommand::Search,
        name: "/search",
        help: "Search messages and tool output in the transcript",
    },
    CommandDescriptor {
        variant: SlashCommand::Hooks,
        name: "/hooks",
        help: "Inspect registered lifecycle hooks",
    },
    CommandDescriptor {
        variant: SlashCommand::Mcp,
        name: "/mcp",
        help: "Manage external tool servers",
    },
    CommandDescriptor {
        variant: SlashCommand::Debug,
        name: "/debug",
        help: "toggle runtime debug logging [/path | off]",
    },
];

/// Build the ALL array from the command table at compile time so ALL and
/// COMMANDS cannot drift apart. Const-eval reads the table by index.
const fn all_variants() -> [SlashCommand; 35] {
    let mut out = [SlashCommand::Login; 35];
    let mut i = 0;
    while i < 35 {
        out[i] = COMMANDS[i].variant;
        i += 1;
    }
    out
}

impl SlashCommand {
    /// All commands in palette order. Derived from the COMMANDS table at
    /// compile time, so the array and the table stay in sync.
    pub const ALL: [SlashCommand; 35] = all_variants();

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
mod tests;
