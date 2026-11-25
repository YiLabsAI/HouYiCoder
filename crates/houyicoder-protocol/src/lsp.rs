//! LSP configuration and status types shared by every layer that names a
//! language server: the config resolver that merges the scopes, the runtime
//! manager that reports state, and the surfaces that render it. Pure data
//! with no transport and no runtime, so the protocol leaf stays free of the
//! client stack.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where one language server's configuration came from, which decides its
/// precedence when two scopes name the same server and whether starting a
/// process from it needs the workspace to be trusted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspConfigScope {
    /// The user's own settings file.
    #[default]
    User,
    /// The workspace's lsp.json file. Requires workspace trust to spawn.
    Project,
    /// A plugin installed under the product's own plugin directory.
    Plugin,
    /// A plugin discovered through a compatible frontend's plugin cache.
    ClaudePlugin,
}

/// How one server is reached. The standard-stream transport is the only
/// one this host launches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LspTransportKind {
    #[default]
    Stdio,
    Socket,
}

/// One resolved language server: its name, the scope it came from, and how
/// to launch and configure it. Field names follow the on-disk format, so
/// a hand-written entry and a programmatic write agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedLspServerConfig {
    /// The server name every later layer keys by.
    #[serde(skip)]
    pub name: String,
    #[serde(skip)]
    pub scope: LspConfigScope,
    /// The executable to launch.
    pub command: String,
    /// Arguments after the command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// File extension to LSP language id, the routing table a file's suffix
    /// consults. Keys are lowercase with the leading dot.
    pub extension_to_language: BTreeMap<String, String>,
    /// How the server is reached. Only the standard-stream transport runs;
    /// a socket entry parses so a resolver can report it instead of
    /// mistaking it for stdio.
    #[serde(default)]
    pub transport: LspTransportKind,
    /// Variables the child needs, already expanded.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Opaque initialization options handed to the server at the handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<Value>,
    /// Settings sent through workspace/didChangeConfiguration after the
    /// handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Value>,
    /// The folder the server treats as its workspace root, when the entry
    /// overrides the session's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_folder: Option<String>,
    /// How long the handshake may take before the launch counts as failed.
    #[serde(default = "default_startup_timeout_ms", rename = "startupTimeout")]
    pub startup_timeout_ms: u64,
    /// How long a graceful shutdown may take before the process is dropped.
    #[serde(default = "default_shutdown_timeout_ms", rename = "shutdownTimeout")]
    pub shutdown_timeout_ms: u64,
    /// Whether an unexpected exit is followed by an automatic relaunch.
    #[serde(default = "default_restart_on_crash")]
    pub restart_on_crash: bool,
    /// How many relaunches one failure streak may spend.
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
    /// False when the entry starts out switched off.
    #[serde(default)]
    pub disabled: bool,
    /// True when an untrusted workspace holds a project entry off, so
    /// enabling it is refused rather than merely undoable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
}

impl ScopedLspServerConfig {
    /// A plain stdio entry over one command, with the production defaults.
    pub fn stdio(
        command: impl Into<String>,
        extension_to_language: BTreeMap<String, String>,
    ) -> Self {
        Self {
            name: String::new(),
            scope: LspConfigScope::User,
            command: command.into(),
            args: Vec::new(),
            extension_to_language,
            transport: LspTransportKind::Stdio,
            env: BTreeMap::new(),
            initialization_options: None,
            settings: None,
            workspace_folder: None,
            startup_timeout_ms: default_startup_timeout_ms(),
            shutdown_timeout_ms: default_shutdown_timeout_ms(),
            restart_on_crash: default_restart_on_crash(),
            max_restarts: default_max_restarts(),
            disabled: false,
            locked: false,
        }
    }

    /// Every extension this server owns, without the leading dot, sorted.
    pub fn extensions(&self) -> Vec<String> {
        let mut extensions: Vec<String> = self
            .extension_to_language
            .keys()
            .map(|key| key.trim_start_matches('.').to_string())
            .collect();
        extensions.sort();
        extensions.dedup();
        extensions
    }
}

/// The lifecycle of one managed server, as the surfaces render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LspServerState {
    Idle,
    Starting,
    /// Running while the server reports background indexing work; results
    /// may still be partial.
    Indexing {
        message: Option<String>,
    },
    Running,
    Failed {
        error: String,
    },
    Disabled {
        reason: String,
    },
}

/// A read-only snapshot of one server, shared by the status surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspServerStatusView {
    pub name: String,
    pub scope: LspConfigScope,
    pub state: LspServerState,
    /// Whether the trust gate holds the switch, so a surface hides the
    /// toggle instead of offering one that cannot act.
    #[serde(default)]
    pub locked: bool,
    /// The command and arguments as configured, for the detail page.
    pub command_line: String,
    /// The extensions this server owns, without the leading dot.
    pub extensions: Vec<String>,
    pub open_documents: usize,
    pub diagnostic_count: usize,
    pub recent_stderr: Vec<String>,
}

/// The operations the lsp tool exposes. Wire names follow the language
/// server protocol, since models have read those names in their training
/// data and a translated spelling is one more thing to get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LspOperation {
    GoToDefinition,
    FindReferences,
    Hover,
    DocumentSymbol,
    WorkspaceSymbol,
    GoToImplementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
}

impl LspOperation {
    /// Parse one wire spelling, case-sensitive to the protocol's own names.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "goToDefinition" => Self::GoToDefinition,
            "findReferences" => Self::FindReferences,
            "hover" => Self::Hover,
            "documentSymbol" => Self::DocumentSymbol,
            "workspaceSymbol" => Self::WorkspaceSymbol,
            "goToImplementation" => Self::GoToImplementation,
            "prepareCallHierarchy" => Self::PrepareCallHierarchy,
            "incomingCalls" => Self::IncomingCalls,
            "outgoingCalls" => Self::OutgoingCalls,
            _ => return None,
        })
    }

    /// Whether the operation reads a call hierarchy, so the tool knows to
    /// resolve the item first.
    pub fn is_call_hierarchy(self) -> bool {
        matches!(
            self,
            Self::PrepareCallHierarchy | Self::IncomingCalls | Self::OutgoingCalls
        )
    }

    /// The position-based operations need a 1-based line and character; the
    /// file-wide ones ignore them.
    pub fn needs_position(self) -> bool {
        !matches!(self, Self::DocumentSymbol | Self::WorkspaceSymbol)
    }
}

/// A zero-based line and UTF-16 code-unit position, the protocol's own
/// coordinate. One-based conversion happens at the tool boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

/// A half-open span between two positions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

/// A file URI, the handle every document-scoped request names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspTextDocument {
    pub uri: String,
}

/// One place a symbol lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspLocation {
    pub uri: String,
    pub range: LspRange,
}

/// A definition answer that also names the span the symbol came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspLocationLink {
    pub target_uri: String,
    #[serde(default)]
    pub target_range: LspRange,
    #[serde(default)]
    pub target_selection_range: LspRange,
    #[serde(default)]
    pub origin_selection_range: Option<LspRange>,
}

/// A hover answer: rendering kinds the client may receive, kept as tagged
/// variants so a plain string and a markup block both parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LspHoverContents {
    Marked { kind: String, value: String },
    MarkedList(Vec<LspHoverContents>),
    Plain(String),
}

/// The hover answer with its optional span.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspHover {
    pub contents: LspHoverContents,
    #[serde(default)]
    pub range: Option<LspRange>,
}

/// One symbol in a document outline. The hierarchical spelling is requested,
/// so children nest under their owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDocumentSymbol {
    pub name: String,
    #[serde(default)]
    pub detail: Option<String>,
    pub kind: u32,
    pub range: LspRange,
    #[serde(default)]
    pub selection_range: Option<LspRange>,
    #[serde(default)]
    pub children: Vec<LspDocumentSymbol>,
}

/// One flat symbol entry, the form a workspace-wide search answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspSymbolInformation {
    pub name: String,
    pub kind: u32,
    pub location: LspLocation,
    #[serde(default)]
    pub container_name: Option<String>,
}

/// One call-hierarchy item: the symbol identity the two-step call flow hands
/// back to the calls request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspCallHierarchyItem {
    pub name: String,
    pub kind: u32,
    pub uri: String,
    pub range: LspRange,
    #[serde(default)]
    pub selection_range: Option<LspRange>,
}

/// One incoming call: the caller and the call sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspIncomingCall {
    pub from: LspCallHierarchyItem,
    #[serde(default)]
    pub from_ranges: Vec<LspRange>,
}

/// One outgoing call: the callee and the call sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspOutgoingCall {
    pub to: LspCallHierarchyItem,
    #[serde(default)]
    pub from_ranges: Vec<LspRange>,
}

/// One diagnostic a server publishes for a file. Severity follows the
/// protocol scale: 1 error, 2 warning, 3 information, 4 hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDiagnostic {
    pub range: LspRange,
    #[serde(default)]
    pub severity: Option<u32>,
    #[serde(default)]
    pub code: Option<Value>,
    #[serde(default)]
    pub source: Option<String>,
    pub message: String,
}

/// The notification a server publishes when a file's diagnostics change. An
/// empty list clears what the file had.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspPublishDiagnostics {
    pub uri: String,
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub diagnostics: Vec<LspDiagnostic>,
}

fn default_startup_timeout_ms() -> u64 {
    10_000
}

fn default_shutdown_timeout_ms() -> u64 {
    2_000
}

fn default_restart_on_crash() -> bool {
    true
}

fn default_max_restarts() -> u32 {
    3
}

#[cfg(test)]
mod tests;
