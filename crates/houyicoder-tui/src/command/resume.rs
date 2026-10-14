//! /resume command: open the session picker, switch by sid/name, or resume
//! from an export file.

use crate::input::queue::InputItem;
use crate::state::{App, Pane};

impl App {
    /// /resume [arg]: with no arg, open the session picker. With an arg:
    /// (a) an existing .json file path resumes from an exported transcript;
    /// (b) a session id or title match switches directly. The session swap
    /// is in-process: enqueue a /resume Command with the resolved target at
    /// the queue head; the event loop's idle drain calls try_switch_session,
    /// which reads that command, invokes the resume_builder to get a new
    /// bundle, swaps the session in-place, and continues the same event loop
    /// (no quit, no restart). An in-process switch-session path.
    pub(crate) fn run_resume(&mut self, arg: Option<&str>) {
        if self.sessions.current.connection.is_none() {
            self.system_line("resume: not connected");
            return;
        }
        // Connected but the session catalog is not installed: a capability
        // gap, not a disconnect.
        let Some(catalog) = self.sessions.catalog.clone() else {
            self.system_line("resume: session history unavailable");
            return;
        };
        let current_sid = self.sessions.current.identity.session_id.0.clone();
        match arg {
            None => {
                let rows = catalog.sessions(&current_sid);
                if rows.is_empty() {
                    self.system_line("resume: no other sessions on disk");
                    return;
                }
                self.ui.navigation.resume_picker.rows = rows;
                self.ui.navigation.resume_picker.open();
                self.ui.navigation.pane = Pane::Resume;
                // Resolve the first visible rows immediately so the picker
                // shows real titles + last-active times on the first render,
                // not placeholders. Subsequent rows resolve a few per frame
                // in the poll loop.
                self.ui
                    .navigation
                    .resume_picker
                    .resolve_rows(catalog.as_ref(), 20);
            }
            Some(query) => {
                if is_export_file_path(query) {
                    self.ui.navigation.resume_picker.close();
                    self.ui
                        .input
                        .queue
                        .insert(0, InputItem::Command(format!("/resume {query}")));
                    self.system_line(self.resume_switch_message(query));
                    return;
                }
                // One-shot lookup: resolve all rows (need real titles for
                // unnamed sessions), then find a match. Slower than the
                // picker open path, but this is an explicit name search,
                // not a list display.
                let mut rows = catalog.sessions(&current_sid);
                for row in &mut rows {
                    catalog.resolve_detail(row);
                }
                let sid = rows
                    .into_iter()
                    .find(|r| {
                        r.sid_str == query
                            || r.title
                                .to_ascii_lowercase()
                                .contains(&query.to_ascii_lowercase())
                    })
                    .map(|r| r.sid_str);
                match sid {
                    Some(s) => {
                        self.ui.navigation.resume_picker.close();
                        self.ui.navigation.pane = Pane::Transcript;
                        self.ui
                            .input
                            .queue
                            .insert(0, InputItem::Command(format!("/resume {s}")));
                        self.system_line(self.resume_switch_message(&s));
                    }
                    None => {
                        self.system_line(format!("resume: no session matches {query:?}"));
                    }
                }
            }
        }
    }
}

/// Recognize an export file path: an existing path that ends in .json. A
/// bare token (a sid or a session name) never matches because it is not an
/// existing .json file, so sid/title matching stays the fallback. Relative
/// paths (./x.json) and absolute paths (/tmp/x.json) both work as long as
/// the file exists.
fn is_export_file_path(arg: &str) -> bool {
    let p = std::path::Path::new(arg);
    p.is_file() && arg.ends_with(".json")
}

/// The one-time message a resume set-point sends when it queues a target.
/// Busy-aware: a run in flight defers the swap (try_switch_session takes the
/// target when the run resolves), so the user is told the switch will happen
/// on completion, not that it is happening now. Fires once per user action
/// (run_resume / picker Enter); the convergence point stays silent.
impl App {
    pub(crate) fn resume_switch_message(&self, label: &str) -> String {
        if self.agent_busy() {
            format!("resume: will switch to {label} when the run finishes")
        } else {
            format!("resume: switching to {label}...")
        }
    }
}

#[cfg(test)]
mod tests;
