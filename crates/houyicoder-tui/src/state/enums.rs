//! Self-contained TUI state enums (screen, stage, pane, viewport mode)
//! extracted from state.rs so the App struct file stays under the file-size
//! gate. Re-exported by state.rs as state::Screen etc., so callers are
//! unchanged.

/// The natural pane for a stage in the guided chain. Used by rewind to land
/// the user back on the pane that matches the stage they returned to.
pub fn pane_for_stage(stage: Stage) -> Pane {
    match stage {
        Stage::Idle => Pane::Transcript,
        Stage::Design => Pane::Spec,
        Stage::Implementing => Pane::Diff,
        Stage::Verify => Pane::Review,
        Stage::Done => Pane::Verify,
    }
}

/// The viewport mode: how much chrome surrounds the content. The viewport
/// tracks the user's cognitive mode, not a static frame. Working = content
/// plus a 1-line status bar and the input box. Focus = progress folded into
/// the pane title, input hidden (implement/verify). Scroll = full-screen
/// transcript read with a 1-line overlay (PgUp).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportMode {
    Working,
    Focus,
    Scroll,
}

impl ViewportMode {
    /// The viewport that matches a stage for auto-transitions. Implement and
    /// verify fold into Focus; idle, design, and done unfold to Working.
    pub fn for_stage(stage: Stage) -> Self {
        match stage {
            Stage::Implementing | Stage::Verify => Self::Focus,
            Stage::Idle | Stage::Design | Stage::Done => Self::Working,
        }
    }
}

/// Which top-level screen the app is showing. The flow is
/// Login -> Working: landing starts with a welcome-back transcript line plus
/// the input box, and typing + Enter pushes transcript lines on the same
/// screen, with no page jump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Login,
    Console,
    Working,
}

/// Where the spec-driven guided chain currently sits. The chain is three
/// stages: design (spec + plan, one approval), implement (per-change diff),
/// and verify (agent review + machine check, one checkpoint). Slash commands
/// move the stage forward; each stage has an artifact and an approval step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    Design,
    Implementing,
    Verify,
    Done,
}

impl Stage {
    /// Short label for the spec context strip.
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Design => "design",
            Self::Implementing => "implementing",
            Self::Verify => "verify",
            Self::Done => "done",
        }
    }

    /// The three guided-chain stages in order, for the progress bar.
    pub const CHAIN: [Stage; 3] = [Stage::Design, Stage::Implementing, Stage::Verify];
}

/// Which content block is currently streaming during a live turn. Set when
/// each delta arrives so the spinner verb reflects the ACTIVE block: a
/// ReasoningDelta flips it to Thinking, an assistant-text Delta flips it to
/// Responding. This is the live-phase signal the verb reads — NOT whether
/// any reasoning has streamed this turn (a sticky once-true test would lock
/// the verb to Thinking for the whole turn even after assistant text takes
/// over). The displayed verbs stay Thinking + Working only; Responding is an
/// internal state name (the non-thinking streaming branch) and is never
/// shown as a word. Reset to None on Done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiveBlock {
    #[default]
    None,
    Thinking,
    Responding,
}

/// A tab enum that cycles left/right through a fixed display order. Each
/// implementer supplies ORDER; next/prev wrap. Shared by the in-pane tab
/// switchers (the /permissions rule tabs, the /memory scope filter) so the
/// Left/Right cycle is one pattern, not per-enum boilerplate.
pub trait CyclicTab: 'static + Sized + Copy + PartialEq + Eq {
    /// Left-to-right display order, for Left/Right cycling.
    const ORDER: &'static [Self];
    /// The next tab to the right, wrapping.
    fn next(self) -> Self {
        let idx = Self::ORDER.iter().position(|t| t == &self).unwrap_or(0);
        let len = Self::ORDER.len();
        Self::ORDER[(idx + 1) % len]
    }
    /// The next tab to the left, wrapping.
    fn prev(self) -> Self {
        let idx = Self::ORDER.iter().position(|t| t == &self).unwrap_or(0);
        let len = Self::ORDER.len();
        Self::ORDER[(idx + len - 1) % len]
    }
}

/// The tab selected in the /permissions rule manager. Three tabs filter the
/// durable rule list by effect (Allow / Ask / Deny); Recently denied shows
/// this session's denial log; Workspace lists the directories the agent may
/// touch beyond the original working directory. Tab order follows the
/// rule manager: denials first, then the three effect filters, then
/// workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionTab {
    Recent,
    Allow,
    Ask,
    Deny,
    Workspace,
}

impl PermissionTab {
    /// The display label shown in the tab header.
    pub fn label(self) -> &'static str {
        match self {
            Self::Allow => "Allow",
            Self::Ask => "Ask",
            Self::Deny => "Deny",
            Self::Recent => "Recently denied",
            Self::Workspace => "Workspace",
        }
    }

    /// Whether this tab shows the durable rule list (and so hosts the
    /// SearchBox + add/remove entry keys). Recent and Workspace render from
    /// other caches and are nav-only.
    pub fn is_rule_tab(self) -> bool {
        matches!(self, Self::Allow | Self::Ask | Self::Deny)
    }
}

impl CyclicTab for PermissionTab {
    const ORDER: &'static [Self] = &[
        Self::Recent,
        Self::Allow,
        Self::Ask,
        Self::Deny,
        Self::Workspace,
    ];
}

/// The active input sub-mode of the /permissions pane. None = navigating the
/// rule list; the other variants drive an Add flow (spec text then a
/// destination pick), a Remove confirm (Yes/No), or a live search filter.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum PermissionInput {
    #[default]
    None,
    /// Type the rule spec (tool [content:]effect); Enter advances to the
    /// destination pick.
    Add,
    /// Pick where the rule persists (project / user / local); Left/Right
    /// cycles, Enter ships the rule with this destination. The parsed spec
    /// (action / content / effect) is carried as pub protocol types so the
    /// enum stays externally nameable without exposing a crate-private spec.
    AddDestination {
        action: String,
        content: Option<houyicoder_protocol::frontend::permission::PermissionRuleContent>,
        effect: houyicoder_protocol::frontend::permission::PermissionEffect,
        destination: houyicoder_protocol::frontend::permission::RuleDestination,
    },
    /// Confirm removing the rule at idx; Left/Right picks Yes/No, Enter fires.
    Remove {
        idx: usize,
        confirm: bool,
    },
    /// Type a directory path to add to the workspace (Workspace tab). Falls
    /// through to the main input box for typing; Enter ships the path to the
    /// server, which canonicalizes + extends the fence.
    AddDir,
    /// Confirm removing the working directory at idx (into dirs_cache);
    /// Left/Right picks Yes/No, Enter fires. Follows Remove but ships a path
    /// removal (the server matches by canonical path).
    RemoveDir {
        idx: usize,
        confirm: bool,
    },
    Search,
}

impl PermissionInput {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Which capability pane is shown in the working surface main area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Transcript,
    Spec,
    Plan,
    Diff,
    Review,
    Verify,
    Graph,
    Memory,
    Agents,
    Artifact,
    /// The interactive permission-rule manager (/permissions).
    Permission,
    /// The linked-worktree manager (/worktrees). This pane lists
    /// the git worktree entries with Enter to enter one and d to remove
    /// one. Remove routes through the agent worktree tool so the approval
    /// gate still fires.
    Worktree,
    /// The /trajectory pane: a turn-organized distributed-trace view with
    /// an ASCII time axis. Shows session summary, turn list, and per-turn
    /// event details with proportional latency bars. Mock data for now —
    /// real wiring arrives when the observability log is connected.
    Trajectory,
    /// The /status pane: identity (version / session name / session id / cwd
    /// / provenance) plus runtime (model / mode / sandbox / breaker) plus
    /// session (duration / tokens / tasks). One tab this round (Session);
    /// the tab-cycle framework is reserved so Usage / Config / Diagnostics
    /// tabs slot in later. Renders inline below the transcript tail through
    /// the shared Pane template, not as a transcript text dump.
    Status,
    /// The /resume picker pane: a filtered session list (relative time +
    /// title + cwd basename). Renders inline below the transcript tail
    /// through the shared Pane template, not as a bottom-anchored popover.
    /// The picker state machine (open / sel / query / filtered) and the
    /// keys (Up / Down / Enter / Esc / char) stay; only the container moves.
    Resume,
    /// The /hooks pane: the registered-hook list. Renders through the shared
    /// Pane template.
    Hooks,
    /// The /skills pane: the discovered-skill list (name, description,
    /// source, body token estimate). Renders through the shared Pane
    /// template. Esc closes.
    Skills,
    /// The /model pane: the selectable model list (Default / Max / Fable /
    /// Pro / Flash). Up / Down navigate, Enter sets the default, s uses this
    /// session only, Esc closes.
    Model,
    /// The /tools pane: registered tool list.
    Tools,
}

/// Storage-scope filter the /memory pane cycles through. All shows every
/// root merged; the others narrow to one physical scope (user / project /
/// auto). Distinct from the per-row source tag (the provenance category) —
/// this is the physical-dimension filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryScopeTab {
    #[default]
    All,
    User,
    Project,
    Auto,
}

impl CyclicTab for MemoryScopeTab {
    const ORDER: &'static [Self] = &[Self::All, Self::User, Self::Project, Self::Auto];
}

impl MemoryScopeTab {
    /// Lowercase label matching the wire scope field, for filtering + render.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::User => "user",
            Self::Project => "project",
            Self::Auto => "auto",
        }
    }
}

/// The active /status sub-tab. Status shows the identity + runtime + session
/// fields; Config shows the sandbox / mode / provider configuration; Usage
/// shows the token breakdown. A Settings-modal-style set of
/// tabs (Status / Config / Usage / Stats — Stats dropped per the design), so
/// /status is one live surface, not a single-focus dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusTab {
    #[default]
    Status,
    Config,
    Usage,
}

impl StatusTab {
    /// The tab title shown in the pane header.
    pub fn title(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Config => "Config",
            Self::Usage => "Usage",
        }
    }
}

impl CyclicTab for StatusTab {
    const ORDER: &'static [Self] = &[Self::Status, Self::Config, Self::Usage];
}

impl Pane {
    /// One-word machine label for the pane (used in logs and tests).
    pub fn label(self) -> &'static str {
        match self {
            Self::Transcript => "log",
            Self::Spec => "spec",
            Self::Plan => "plan",
            Self::Diff => "diff",
            Self::Review => "review",
            Self::Verify => "verify",
            Self::Graph => "graph",
            Self::Memory => "memory",
            Self::Agents => "agents",
            Self::Artifact => "artifact",
            Self::Permission => "permission",
            Self::Worktree => "worktrees",
            Self::Trajectory => "trajectory",
            Self::Status => "status",
            Self::Resume => "resume",
            Self::Hooks => "hooks",
            Self::Skills => "skills",
            Self::Model => "model",
            Self::Tools => "tools",
        }
    }

    /// The Tab-cycle order: the six primary guided-chain panes only. Graph,
    /// Memory, and Agents are utility panes reached via slash commands, not Tab.
    pub const PRIMARY: [Pane; 6] = [
        Pane::Transcript,
        Pane::Spec,
        Pane::Plan,
        Pane::Diff,
        Pane::Review,
        Pane::Verify,
    ];

    /// All panes in render order: the six primary guided-chain panes followed
    /// by the slash-only utility panes (graph, memory, agents) and artifact.
    /// Used to iterate every pane in tests.
    pub const CYCLE: [Pane; 9] = [
        Pane::Transcript,
        Pane::Spec,
        Pane::Plan,
        Pane::Diff,
        Pane::Review,
        Pane::Verify,
        Pane::Graph,
        Pane::Memory,
        Pane::Agents,
    ];
}
