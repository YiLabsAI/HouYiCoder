//! /resume command: open the session picker, switch by sid/name, or resume
//! from an export file. Split out of command.rs on size grounds.

use crate::state::{App, Pane};

impl App {
    /// /resume [arg]: with no arg, open the session picker. With an arg:
    /// (a) an existing .json file path resumes from an exported transcript;
    /// (b) a session id or title match switches directly. The session swap
    /// is in-process: set pending_resume_target; the event loop calls the
    /// resume_builder to get a new bundle, swaps the session in-place, and
    /// continues the same event loop (no quit, no restart). An in-process
    /// switch-session path.
    pub(crate) fn run_resume(&mut self, arg: Option<&str>) {
        let Some(lister) = self.session_lister.clone() else {
            self.system_line("resume: no session store wired (stub mode)");
            return;
        };
        let current_sid = self.session_id.0.clone();
        match arg {
            None => {
                let rows = lister.list_sessions(&current_sid);
                if rows.is_empty() {
                    self.system_line("resume: no other sessions on disk");
                    return;
                }
                self.resume_picker.rows = rows;
                self.resume_picker.open();
                self.pane = Pane::Resume;
                // Resolve the first visible rows immediately so the picker
                // shows real titles + last-active times on the first render,
                // not placeholders. Subsequent rows resolve a few per frame
                // in the poll loop.
                let visible = self.resume_picker.rows.len().min(20);
                for i in 0..visible {
                    if !self.resume_picker.resolved.contains(&i) {
                        let lister = self.session_lister.clone().unwrap();
                        lister.resolve_detail(&mut self.resume_picker.rows[i]);
                        self.resume_picker.resolved.insert(i);
                    }
                }
            }
            Some(query) => {
                if is_export_file_path(query) {
                    self.resume_picker.close();
                    self.pending_resume_target = Some(query.to_string());
                    self.system_line(self.resume_switch_message(query));
                    return;
                }
                // One-shot lookup: resolve all rows (need real titles for
                // unnamed sessions), then find a match. Slower than the
                // picker open path, but this is an explicit name search,
                // not a list display.
                let mut rows = lister.list_sessions(&current_sid);
                for row in &mut rows {
                    lister.resolve_detail(row);
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
                        self.resume_picker.close();
                        self.pane = Pane::Transcript;
                        self.pending_resume_target = Some(s.clone());
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
/// Busy-aware: a run in flight defers the swap (try_swap_session takes the
/// target when the run resolves), so the user is told the switch will happen
/// on completion, not that it is happening now. Fires once per user action
/// (run_resume / picker Enter); the convergence point stays silent.
impl App {
    pub(crate) fn resume_switch_message(&self, label: &str) -> String {
        if self.agent_busy {
            format!("resume: will switch to {label} when the run finishes")
        } else {
            format!("resume: switching to {label}...")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pending_queue::PendingItem;
    use crate::resume_picker::SessionRow;
    use std::sync::Arc;

    struct EmptyLister;
    impl crate::resume_picker::SessionLister for EmptyLister {
        fn list_sessions(&self, _current: &str) -> Vec<SessionRow> {
            Vec::new()
        }
        fn resolve_detail(&self, _row: &mut SessionRow) {}
    }

    struct TwoRowLister;
    impl crate::resume_picker::SessionLister for TwoRowLister {
        fn list_sessions(&self, _current: &str) -> Vec<SessionRow> {
            vec![
                SessionRow {
                    sid_str: "aaaa1111".into(),
                    title: "fix login".into(),
                    cwd_basename: "app".into(),
                    last_active: 1,
                    ..Default::default()
                },
                SessionRow {
                    sid_str: "bbbb2222".into(),
                    title: "port tui".into(),
                    cwd_basename: "app".into(),
                    last_active: 2,
                    ..Default::default()
                },
            ]
        }
        fn resolve_detail(&self, _row: &mut SessionRow) {}
    }

    /// /resume with no lister wired reports stub mode and never opens a pane.
    #[test]
    fn test_no_lister_reports_stub() {
        let mut app = crate::composition::app();
        app.run_resume(None);
        assert!(app.resume_picker.rows.is_empty());
        assert!(!app.resume_picker.open);
        assert!(app.pending_resume_target.is_none());
    }

    /// /resume with no arg and no other sessions reports empty + no pane.
    #[test]
    fn test_empty_store_reports_none() {
        let mut app = crate::composition::app();
        app.session_lister = Some(Arc::new(EmptyLister));
        app.run_resume(None);
        assert!(!app.resume_picker.open);
        assert!(app.pending_resume_target.is_none());
    }

    /// /resume with no arg and sessions opens the Resume pane.
    #[test]
    fn test_opens_pane_with_rows() {
        let mut app = crate::composition::app();
        app.session_lister = Some(Arc::new(TwoRowLister));
        app.run_resume(None);
        assert!(app.resume_picker.open);
        assert_eq!(app.resume_picker.rows.len(), 2);
        assert_eq!(app.pane, crate::state::Pane::Resume);
    }

    /// /resume with a sid arg matches a row by sid and sets the pending target.
    #[test]
    fn test_matches_sid_directly() {
        let mut app = crate::composition::app();
        app.session_lister = Some(Arc::new(TwoRowLister));
        app.run_resume(Some("aaaa1111"));
        assert_eq!(app.pending_resume_target.as_deref(), Some("aaaa1111"));
        assert!(!app.quit, "in-process swap does not quit");
    }

    /// /resume with a title substring matches case-insensitively.
    #[test]
    fn test_matches_title_substring() {
        let mut app = crate::composition::app();
        app.session_lister = Some(Arc::new(TwoRowLister));
        app.run_resume(Some("LOGIN"));
        assert_eq!(app.pending_resume_target.as_deref(), Some("aaaa1111"));
    }

    /// /resume with a nonexistent token reports no match and does not quit.
    #[test]
    fn test_no_match_reports_error() {
        let mut app = crate::composition::app();
        app.session_lister = Some(Arc::new(TwoRowLister));
        app.run_resume(Some("zzz"));
        assert!(app.pending_resume_target.is_none());
        assert!(!app.quit);
    }

    /// A mid-run /resume <sid> defers: it is enqueued as a Command (not
    /// executed now), drained FIFO at idle so it does not fight the run's
    /// writes. The target is NOT set at enqueue time -- the drain calls
    /// run_resume (which sets the target) when the run resolves. A bare
    /// /resume (no arg) still opens the picker (read-only browsing).
    #[test]
    fn test_busy_run_defers_resume() {
        let mut app = crate::composition::app();
        app.session_lister = Some(Arc::new(TwoRowLister));
        app.agent_busy = true;
        app.screen = crate::state::Screen::Working;
        // /resume <sid> mid-run -> enqueued as a Command, not executed.
        app.input.set("/resume aaaa1111".to_string());
        app.submit_input();
        assert_eq!(
            app.pending,
            vec![PendingItem::Command("/resume aaaa1111".into())],
            "/resume <sid> mid-run enqueued as a Command"
        );
        assert!(
            app.pending_resume_target.is_none(),
            "target not set at enqueue (the drain sets it at idle)"
        );
        assert!(app.agent_busy, "the run is still in progress");
    }

    /// A bare /resume (no arg) mid-run does NOT defer -- it opens the picker
    /// (read-only browsing is harmless). Only a specific target defers.
    #[test]
    fn test_busy_bare_opens_picker() {
        let mut app = crate::composition::app();
        app.session_lister = Some(Arc::new(TwoRowLister));
        app.agent_busy = true;
        app.screen = crate::state::Screen::Working;
        app.input.set("/resume".to_string());
        app.submit_input();
        assert!(
            app.pending.is_empty(),
            "bare /resume does not enqueue (picker opens instead)"
        );
        assert!(app.resume_picker.open, "picker opens for browsing");
    }

    /// A non-existent .json path is NOT treated as an export file.
    #[test]
    fn test_missing_json_not_export() {
        assert!(!is_export_file_path("./nonexistent-export.json"));
        assert!(!is_export_file_path("abc123"));
    }
}
