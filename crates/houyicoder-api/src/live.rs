//! Live (in-flight) events the runner emits to a frontend sink while a turn
//! streams. These are NOT the durable TurnEvents (those live in the session
//! log); LiveEvent is the ephemeral, real-time channel from the streaming
//! model call to whatever host wants token-by-token display.
//!
//! The seam is a plain callable (Arc<dyn Fn(&LiveEvent) + Send + Sync>) so the
//! runner stays frontend-agnostic: it knows nothing about the TUI's
//! AgentMessage type or channel. The host adapts LiveEvent into its own
//! render path. This is the "loop translates to AgentMessage" boundary from
//! the streaming design.
//!
//! These types live in ports (not core) so a frontend (the TUI) can import
//! them without pulling in the engine crate. Core re-exports them.

/// One real-time event from an in-flight streaming turn. Ephemeral — the
/// durable record is the TurnEvent log; this is the live preview of it.
///
/// MemorySaved is the exception to in-flight streaming: a background
/// extract/dream task fires it once on completion (after the turn ended) to
/// surface how many memories were written without the user opening the
/// memory pane. It rides the same sink so no second push channel is needed.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveEvent {
    /// One incremental chunk of assistant text. The host appends to its live
    /// assistant line; the authoritative AssistantMessage replaces it when
    /// the turn lands.
    AssistantDelta { text: String },
    /// One incremental chunk of model reasoning. The host appends to its
    /// live reasoning preview; the durable Reasoning event replaces it
    /// when the turn lands.
    ReasoningDelta { text: String },
    /// A background memory task wrote the given count of entries this pass.
    /// Fired once per pass on completion (extract: per fork pass plus the
    /// main-agent saved-this-turn skipped path; dream: per consolidation).
    /// The kind tells the host which verb to render (extract = Saved, dream
    /// = Improved) so the wording decision stays in the frontend, not the
    /// engine.
    MemorySaved {
        count: u32,
        kind: houyicoder_protocol::frontend::memory::MemorySavedKind,
    },
    /// A long-running tool (currently bash) reports its elapsed seconds so
    /// the host can show the chip is making progress, not stuck. The runner
    /// ticks this every ~1s while the tool executes; the authoritative
    /// tool-result frame supersedes it on completion. Carries the call_id so
    /// the host routes the suffix to the right chip. lines is the running
    /// stdout newline count when the backend streams stdout (None when no
    /// streaming — the host shows "(Ns)" only, not "(Ns · M lines)").
    ToolProgress {
        call_id: String,
        elapsed_secs: u64,
        lines: Option<u64>,
    },
    /// A runtime notice the agent loop wants surfaced to the user (not a
    /// delta, not a tool frame). Currently used when a provider rejects an
    /// over-long request without naming its limit — the one moment we know
    /// the catalog's window estimate is wrong AND cannot self-heal — pointing
    /// the user at the catalog override. Best-effort: a host that does not
    /// model this variant ignores it.
    SystemLine { text: String },
    /// One turn-boundary progress snapshot, emitted between turns when the
    /// loop continues. Coarser than the deltas above: not a token stream but
    /// a per-turn summary (cumulative tokens, the turn's tool count, the last
    /// tool name) for a watcher that does not consume the token stream — a
    /// spawned child's bus bridge forwarding to a parent pill. A host that
    /// does not model this variant ignores it (the parent's own delta sink
    /// skips it).
    TurnBoundary {
        turn: u32,
        cumulative_tokens: u64,
        tool_uses: u32,
        last_activity: Option<String>,
    },
}

/// The host-installed sink the runner notifies per LiveEvent. A plain
/// callable (not a channel) so the runner stays frontend-agnostic. The
/// host adapts LiveEvent into its own render path inside the closure.
/// Arc-shared so the runner (itself Arc-shared across spawned tasks) can
/// hold and invoke it without interior mutability.
pub type LiveSink = std::sync::Arc<dyn Fn(&LiveEvent) + Send + Sync>;

/// Re-export the shared saved-kind token so the engine (core, which depends
/// on api) can build a MemorySaved event without importing the wire crate
/// directly. The definition lives in the wire crate so both the engine live
/// event and the wire frontend event share one typed token (no second enum
/// to drift).
pub use houyicoder_protocol::frontend::memory::MemorySavedKind;
