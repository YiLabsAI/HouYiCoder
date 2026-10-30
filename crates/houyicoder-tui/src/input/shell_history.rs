//! Shell history completion for the dollar direct-shell mode. A port of the
//! prompt history's shell-completion algorithm: an in-memory cache of the
//! recent unique dollar commands, refreshed from the prompt-history JSONL with
//! a TTL, prepended on submit, and searched by exact prefix to render a ghost
//! suffix. The cache lives on the Input value rather than as module state, so
//! each composer owns its own and a session switch starts clean.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use super::history;

/// The trigger prefix that marks a prompt-history entry as a direct-shell
/// command. The prompt history stores the full submitted line, so a dollar-mode
/// command is appended as the prefix plus the command and read back by
/// stripping it.
pub const SHELL_PREFIX: &str = "$ ";

/// The most unique commands the cache holds. Matches the prompt-history cap the
/// algorithm was tuned to.
const MAX_COMMANDS: usize = 50;

/// How long the cache stays fresh without a re-read. History only changes when
/// a command is submitted, so a long TTL is safe while typing.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// The completion for the current input: the full command it came from and the
/// suffix to render as ghost text after the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellHistoryMatch {
    pub full_command: String,
    pub suffix: String,
}

/// An in-memory cache of the recent unique dollar commands, refreshed from the
/// prompt-history JSONL. The cache is populated lazily on the first lookup and
/// invalidated by a TTL or a submit (which prepends the new command in place).
#[derive(Default)]
pub(crate) struct ShellHistory {
    cache: Vec<String>,
    loaded_at: Option<Instant>,
    /// The history file path injected by the composition root. Empty means no
    /// history is wired; lookups answer None.
    path: String,
    project: String,
    session: String,
}

impl ShellHistory {
    /// Wire the prompt-history file the cache reads from.
    pub(crate) fn configure(&mut self, path: String, project: String, session: String) {
        self.path = path;
        self.project = project;
        self.session = session;
        self.invalidate();
    }

    /// Drop the cache so the next lookup re-reads the file.
    pub(crate) fn invalidate(&mut self) {
        self.cache.clear();
        self.loaded_at = None;
    }

    /// Prepend a submitted command to the cache without a re-read, deduping so
    /// the most recent run of a repeated command wins. A no-op when the cache
    /// has not been populated yet: the next lookup reads the file, which the
    /// submit already appended to.
    pub(crate) fn prepend(&mut self, command: &str) {
        if self.loaded_at.is_none() {
            return;
        }
        if let Some(idx) = self.cache.iter().position(|c| c == command) {
            self.cache.remove(idx);
        }
        self.cache.insert(0, command.to_string());
        self.cache.truncate(MAX_COMMANDS);
    }

    /// Find the first command in history that starts with the exact input
    /// (spaces included) and is not the input itself. Returns None for input
    /// shorter than two characters or an empty trim, so a lone dollar or a
    /// single keystroke does not surface a completion.
    pub(crate) fn completion(&mut self, input: &str, now: Instant) -> Option<ShellHistoryMatch> {
        if input.len() < 2 || input.trim().is_empty() {
            return None;
        }
        self.refresh_if_stale(now);
        for command in &self.cache {
            if command.starts_with(input) && command != input {
                return Some(ShellHistoryMatch {
                    suffix: command[input.len()..].to_string(),
                    full_command: command.clone(),
                });
            }
        }
        None
    }

    fn refresh_if_stale(&mut self, now: Instant) {
        let fresh = self
            .loaded_at
            .is_some_and(|loaded| now.duration_since(loaded) < CACHE_TTL);
        if fresh {
            return;
        }
        self.cache = read_shell_commands(&self.path, &self.project, &self.session);
        self.loaded_at = Some(now);
    }
}

/// Read the prompt-history JSONL and return the unique dollar commands, newest
/// first, capped at MAX_COMMANDS. Entries that are not prefix-prefixed are
/// prompt turns and are skipped; the prefix is stripped so the cache holds
/// bare commands. Mirrors the prompt-history read's project filter and skip-set.
fn read_shell_commands(path: &str, project: &str, session: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    let entries = history::read(project, session, &Default::default(), Path::new(path));
    let mut commands: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for display in entries {
        let Some(command) = display.strip_prefix(SHELL_PREFIX) else {
            continue;
        };
        let command = command.trim();
        if command.is_empty() || seen.contains(command) {
            continue;
        }
        seen.insert(command.to_string());
        commands.push(command.to_string());
        if commands.len() >= MAX_COMMANDS {
            break;
        }
    }
    commands
}
