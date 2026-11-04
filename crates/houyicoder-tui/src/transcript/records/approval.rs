//! Tool-approval prompt state and option layout rules.

use std::time::{Duration, Instant};

use houyicoder_protocol::acp_wire::{PermissionOption, PermissionOptionKind};
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::frontend::permission::AskSource;
use houyicoder_protocol::frontend::run::{ApprovalScope, DelegationSource};

/// Tool-approval prompt state. Three options: approve, reject, or
/// always-allow (approve + add a permission rule so the same tool no
/// longer prompts). call_id links the prompt back to the
/// ApprovalRequest from the agent loop so resume() gets the right decision.
#[derive(Debug, Clone, Default)]
pub struct Approval {
    /// Tool name awaiting approval.
    pub tool: String,
    /// Serialized arguments (placeholder).
    pub args: String,
    /// Why the tool wants to run: the structured AskReason detail the gate
    /// produced, or a generic prompt when the ask carried no reason.
    pub reason: String,
    /// The source of the ask, when the wire carried a reason. Drives the
    /// card: a SystemSafety source hides the "remember" option because
    /// consent cannot override a protected-path check.
    pub source: Option<AskSource>,
    /// The gate validator that produced the ask, when the wire carried a
    /// reason. Drives the auto-approve countdown: a card from a validator
    /// that widens the workspace bounds, touches the network, or forces a
    /// push never auto-approves.
    pub validator: Option<String>,
    /// The child delegation this ask was routed up from, when the ask
    /// originates from a spawned child rather than the parent's own run.
    /// None for a parent tool call. The card labels the child type so the
    /// user can tell a child's ask from the parent's when delegations are
    /// in flight.
    pub delegation: Option<DelegationSource>,
    /// An optional note from the containment layer the card renders as a
    /// fourth line when present.
    pub containment_note: Option<String>,
    /// The index of the currently highlighted option (0-based).
    pub selected: usize,
    /// Whether the argument block asks for the whole of its content instead of
    /// the bounded head and tail. The bound leaves the question and the
    /// options their rows; expanding trades those rows back for the part the
    /// bound released, so even an expanded block still gives way while the
    /// card is short.
    pub expanded: bool,
    /// The agent-loop call_id this popup resolves.
    pub call_id: String,
    /// The verdict options the server offered (N-option). Empty when the
    /// server deferred; the card falls back to its built-in 3-option set.
    pub options: Vec<PermissionOption>,
    /// The instant a Level 1 auto-approve countdown expires, when one is
    /// running. None when the card is not counting down: a Level 2 ask, a
    /// cancelled countdown, or a delay of zero.
    pub auto_approve_at: Option<Instant>,
    /// The whole-second countdown the last frame drew. The card holds still
    /// between ticks, so the shell redraws only when this figure changes;
    /// comparing deadlines instead would never report the per-second step.
    pub shown_secs: Option<u64>,
    /// Whether the user touched a navigation key while the countdown ran.
    /// One touch cancels the auto-approve for the life of this card; the
    /// user's Enter or Esc then decides it.
    pub auto_approve_cancelled: bool,
}

/// The number of visible options in the approval prompt.
pub const APPROVAL_OPTIONS: usize = 3;

impl Approval {
    /// The scope the server attached to its remember option, when it offered
    /// one. The server is the authority on what a yes-always installs, so the
    /// card shows that text instead of deriving one from the command.
    pub fn remember_name(&self) -> Option<&str> {
        self.options
            .iter()
            .find(|option| matches!(option.kind, PermissionOptionKind::AllowAlways))
            .map(|option| option.name.as_str())
            .filter(|name| !name.is_empty())
    }

    /// Whether the "Yes, and don't ask again" option is hidden. A protected-
    /// path check (source SystemSafety) is authoritative: a stored consent
    /// rule cannot override it, so persisting a yes-always would be a no-op
    /// at best and misleading at worst. A server that sent an explicit option
    /// set is equally authoritative: when it offered no remember option, it
    /// can install no scope for this call.
    pub fn remember_hidden(&self) -> bool {
        if matches!(self.source, Some(AskSource::SystemSafety)) {
            return true;
        }
        !self.options.is_empty() && self.remember_name().is_none()
    }

    /// Whether this is an entitlement ask (a deny-log discovery offering
    /// to authorize blocked mach services for a skill).
    pub fn is_entitlement(&self) -> bool {
        self.tool == ENTITLEMENT_TOOL
    }

    /// Whether the card renders a two-option form: a protected-path ask
    /// hides persistence, while an entitlement ask offers a persistent
    /// allow or a decline.
    pub fn two_option_card(&self) -> bool {
        self.remember_hidden() || self.is_entitlement()
    }

    /// Whether this card may auto-approve in Auto mode after the countdown.
    /// Level 2 asks never do: a protected path, a request to widen the
    /// workspace bounds, a network call, a forced push, or a native tool call
    /// the user did not flag as discarding all need a human press. The rest
    /// (a fenced compound command, a destructive command, a local git
    /// checkpoint, a discard-flagged native call) recover or stay inside the
    /// fence, so a countdown that lands on Yes-once is safe.
    pub fn auto_approve_eligible(&self) -> bool {
        if matches!(self.source, Some(AskSource::SystemSafety)) || self.is_entitlement() {
            return false;
        }
        match self.validator.as_deref() {
            Some(
                "path_bounds" | "path-bounds" | "directory_capability" | "directory-capability",
            ) => {
                return false;
            }
            // A read that the fence already contains may land on yes
            // unattended; sending the user's work to a remote may not.
            Some("network_egress" | "network-egress") => {
                return !self.egress_sends_work();
            }
            Some("git_checkpoint") => return !self.is_forced_push(),
            Some("tool_native") => return self.discards_changes(),
            _ => {}
        }
        true
    }

    /// Whether the network access this ask describes sends local work out.
    ///
    /// A read is recognized positively: every segment of the command must be a
    /// form this walk knows to read. Anything it does not recognize is held for
    /// a human, because a countdown that publishes the user's work is the
    /// failure this exists to prevent.
    fn egress_sends_work(&self) -> bool {
        !reads_network(&self.egress_command())
    }

    /// The command a shell ask carries, read out of the serialized arguments.
    ///
    /// The classifiers work on a command line, not on the argument record, so
    /// the two are not interchangeable. A record that does not parse falls back
    /// to its own text, which still contains the command.
    fn egress_command(&self) -> String {
        serde_json::from_str::<serde_json::Value>(&self.args)
            .ok()
            .and_then(|value| {
                value
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| self.args.clone())
    }

    /// Whether the serialized args name a forced push, the one git checkpoint
    /// form that rewrites published history rather than local state.
    fn is_forced_push(&self) -> bool {
        let args = self.args.to_ascii_lowercase();
        args.contains("push")
            && (args.contains("--force")
                || args.contains("--no-verify")
                || args.contains(" -f ")
                || args.ends_with(" -f")
                || args.contains(" -f\n"))
    }

    /// Whether the serialized args carry an explicit discard_changes true, the
    /// flag a native tool call uses to say it throws uncommitted work away.
    fn discards_changes(&self) -> bool {
        self.args.contains("\"discard_changes\"") && self.args.contains("true")
    }

    /// Start the auto-approve countdown if the card is eligible and the delay
    /// is non-zero. A Level 2 card, a zero delay, or a cursor parked on No by a
    /// sticky reject leaves the deadline None so the card waits for the user.
    pub fn start_auto_approve(&mut self, delay_secs: u64) {
        if delay_secs > 0 && self.focused_approves() && self.auto_approve_eligible() {
            self.auto_approve_at = Some(Instant::now() + Duration::from_secs(delay_secs));
            self.auto_approve_cancelled = false;
            // A new deadline makes the recorded figure describe a countdown
            // that no longer exists. Leaving it in place means a card that is
            // cancelled and re-armed with the same delay never repaints: the
            // opening tick matches the stale figure, so the countdown appears
            // frozen at a number belonging to the run the user cancelled.
            self.shown_secs = None;
        }
    }

    /// Cancel the countdown for the life of this card. One navigation key does
    /// it; the user's Enter or Esc then decides.
    pub fn cancel_auto_approve(&mut self) {
        self.auto_approve_at = None;
        self.auto_approve_cancelled = true;
    }

    /// The whole seconds left on the countdown, for the "(auto in Ns)" hint.
    /// None when no countdown is running.
    pub fn auto_approve_secs_left(&self, now: Instant) -> Option<u64> {
        let deadline = self.auto_approve_at?;
        let left = deadline.saturating_duration_since(now).as_secs();
        Some(left)
    }

    /// Record the countdown figure this frame will draw. Returns true when it
    /// differs from the figure the last frame drew, which is the signal the
    /// shell turns into a redraw. A card without a countdown reports false
    /// and leaves the recorded figure alone.
    pub fn note_countdown(&mut self, now: Instant) -> bool {
        let Some(left) = self.auto_approve_secs_left(now) else {
            return false;
        };
        if self.shown_secs == Some(left) {
            return false;
        }
        self.shown_secs = Some(left);
        true
    }

    /// Whether the countdown has run down. The caller auto-approves Once when
    /// this returns true and clears the deadline.
    pub fn auto_approve_expired(&self, now: Instant) -> bool {
        self.auto_approve_at.is_some_and(|d| now >= d)
    }

    /// The count of visible options: two (Yes / No) on a two-option card,
    /// otherwise the built-in three.
    pub fn visible_option_count(&self) -> usize {
        if self.two_option_card() {
            2
        } else {
            APPROVAL_OPTIONS
        }
    }

    /// Label for the currently focused button.
    pub fn focus_label(&self) -> &'static str {
        match self.selected {
            0 => "yes",
            1 => "no",
            _ => "yes-dont-ask-again",
        }
    }

    /// True when the focused option approves the tool call.
    pub fn focused_approves(&self) -> bool {
        self.selected != 1
    }

    /// True when the focused option is Yes-don't-ask-again.
    pub fn focused_persists(&self) -> bool {
        self.selected == 2
    }

    /// The identity of the focused option for the built-in 3-option set.
    pub fn focused_kind(&self) -> PermissionOptionKind {
        match self.selected {
            0 => PermissionOptionKind::AllowOnce,
            1 => PermissionOptionKind::RejectOnce,
            _ => PermissionOptionKind::AllowAlways,
        }
    }

    /// The lifetime the server declared for the focused option, when it
    /// declared one. The server computed it from the ask's plan, so a client
    /// reads it instead of guessing; a card the server did not fill in has no
    /// declaration and the caller falls back to its own reading.
    pub fn focused_scope(&self) -> Option<ApprovalScope> {
        let kind = self.focused_kind();
        self.options
            .iter()
            .find(|option| option.kind == kind)
            .and_then(|option| option.scope)
    }

    /// The lifetime the remember option declares, independent of which row is
    /// focused. The card prints it as a tail on that row and must keep it
    /// readable when the row is clipped.
    pub fn remember_scope(&self) -> Option<ApprovalScope> {
        self.options
            .iter()
            .find(|option| matches!(option.kind, PermissionOptionKind::AllowAlways))
            .and_then(|option| option.scope)
    }

    /// The built-in 3-option index whose kind matches.
    pub fn index_for_kind(kind: PermissionOptionKind) -> usize {
        match kind {
            PermissionOptionKind::AllowOnce => 0,
            PermissionOptionKind::AllowAlways => 2,
            PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => 1,
        }
    }
}

mod formatter;

use formatter::without_formatter_code;

#[cfg(test)]
mod tests;

/// Whether the network access in a command only reads.
///
/// A command is a list of segments, and only some of them reach the network.
/// A leading change of directory, a formatter at the end of a pipe, or a
/// redirect that moves a stream says nothing about the destination, so each is
/// skipped rather than read as an unrecognized command. What remains must be a
/// recognized read, and the command must contain at least one: a send, an
/// interactive transport, or a segment this walk does not know is held.
fn reads_network(command: &str) -> bool {
    let mut saw_network_read = false;
    // An interpreter's inline code carries the separators the split below cuts
    // on, so it is read and set aside first. A body that is not a formatter
    // stops the walk: the countdown resolves unattended, and an opaque program
    // beside a request does not get one.
    let Some(without_code) = without_formatter_code(command) else {
        return false;
    };
    let stripped = without_stream_redirects(&without_code);
    // The command keeps its case: a short flag is case-sensitive, and folding
    // it would read an upload flag as the fail flag beside it.
    for segment in stripped.split(['&', ';', '|', '\n', '(', ')', '`']) {
        let words: Vec<&str> = segment
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| c == '"' || c == '\''))
            .filter(|w| !w.is_empty() && !is_assignment(w))
            .collect();
        let Some(&first) = words.first() else {
            continue;
        };
        let args = &words[1..];
        match first.to_ascii_lowercase().as_str() {
            "git" => match git_subcommand(args) {
                "push" => return false,
                "pull" | "fetch" | "clone" | "ls-remote" => saw_network_read = true,
                // A local repository verb says nothing about a destination.
                _ => {}
            },
            "curl" | "wget" => {
                if words.iter().any(|w| sends_body(w)) || names_write_method(&words) {
                    return false;
                }
                saw_network_read = true;
            }
            "gh" => {
                let group = words.get(1).copied().unwrap_or("");
                let verb = words.get(2).copied().unwrap_or("");
                let reads = matches!(group, "version" | "help" | "status" | "search")
                    || (group == "auth" && verb == "status")
                    || (matches!(
                        group,
                        "pr" | "issue"
                            | "repo"
                            | "release"
                            | "run"
                            | "workflow"
                            | "label"
                            | "cache"
                    ) && matches!(verb, "view" | "list" | "diff" | "checks" | "status"));
                if !reads {
                    return false;
                }
                saw_network_read = true;
            }
            // An interactive transport carries whatever the far side asks for.
            "scp" | "rsync" | "sftp" | "ssh" | "nc" | "netcat" | "telnet" => return false,
            "npm" | "cargo" if args.first() == Some(&"publish") => return false,
            "pip" if args.first() == Some(&"upload") => return false,
            local if is_local_consumer(local) => {}
            // Not a form this walk knows. A countdown that publishes work is
            // the failure this exists to prevent, so it is held.
            _ => return false,
        }
    }
    saw_network_read
}

/// Remove the redirects that only move a stream.
///
/// The split below cuts on a bare ampersand, which would turn 2>&1 into a
/// segment of its own and read it as a command. Removing the stream forms
/// first keeps the split on the command separators it is meant for.
fn without_stream_redirects(command: &str) -> String {
    let mut out = command.to_string();
    for form in [
        "2>&1",
        "1>&2",
        "&>/dev/null",
        "2>/dev/null",
        ">/dev/null",
        "2>&-",
    ] {
        out = out.replace(form, " ");
    }
    out
}

/// Whether a request names a method that writes.
///
/// A request body is not the only way to write on the far side: a method on
/// its own says so. The read methods are the ones that leave the far side
/// unchanged.
fn names_write_method(words: &[&str]) -> bool {
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        index += 1;
        let value = word
            .strip_prefix("--request=")
            .map(str::to_string)
            .or_else(|| {
                let rest = word
                    .strip_prefix("--request")
                    .or_else(|| word.strip_prefix("-X"))?;
                if !rest.is_empty() {
                    return Some(rest.trim_start_matches('=').to_string());
                }
                let next = words.get(index).copied().unwrap_or("");
                index += 1;
                Some(next.to_string())
            });
        if let Some(value) = value {
            let method = value.trim().to_ascii_uppercase();
            if !method.is_empty() && !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS") {
                return true;
            }
        }
    }
    false
}

/// Whether a word is a leading NAME=value assignment rather than a command.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a segment is a local command that only reads.
///
/// The set is the readers a shell user puts beside a request: a change of
/// directory, a formatter at the end of a pipe, a filter. An interpreter is
/// absent on purpose. Code handed to one can reach the network through a
/// library, which no command scan sees.
///
/// A countdown resolves unattended, so an interpreter beside a request is held
/// like any other form this walk cannot read. What the user gets instead is a
/// remembered answer scoped to that exact command, which costs one press rather
/// than one per run.
fn is_local_consumer(word: &str) -> bool {
    matches!(
        word,
        "cd" | "echo"
            | "printf"
            | "head"
            | "tail"
            | "cat"
            | "jq"
            | "grep"
            | "egrep"
            | "fgrep"
            | "rg"
            | "sed"
            | "awk"
            | "wc"
            | "sort"
            | "uniq"
            | "cut"
            | "tr"
            | "true"
            | "false"
            | "test"
            | "ls"
            | "pwd"
            | "basename"
            | "dirname"
            | "realpath"
            | "date"
            | "stat"
            | "file"
            | "strings"
            | "shasum"
            | "sha256sum"
            | "md5"
            | "diff"
            | "which"
            | "whereis"
            | "whoami"
            | "id"
            | "uname"
            | "df"
            | "du"
            | "groups"
            | "seq"
            | "sleep"
    )
}

/// The subcommand a git invocation runs, past the flags that precede it.
///
/// The flags that take a value are named rather than guessed: skipping only the
/// flag would take its value for the subcommand and read a push as whatever
/// that value happened to be.
fn git_subcommand<'a>(args: &[&'a str]) -> &'a str {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if arg.starts_with('-') {
            index += if matches!(arg, "-C" | "-c" | "--git-dir" | "--work-tree") {
                2
            } else {
                1
            };
            continue;
        }
        return arg;
    }
    ""
}

/// Whether an argument turns a curl or wget request into an upload or a write
/// on the far side.
fn sends_body(word: &str) -> bool {
    if let Some(long) = word.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or(long);
        return matches!(
            name,
            "data"
                | "data-raw"
                | "data-binary"
                | "data-urlencode"
                | "form"
                | "upload-file"
                | "json"
                | "post-data"
                | "post-file"
                | "body-data"
        );
    }
    let Some(short) = word.strip_prefix('-') else {
        return false;
    };
    !short.starts_with('-') && short.chars().any(|c| matches!(c, 'd' | 'F' | 'T'))
}
