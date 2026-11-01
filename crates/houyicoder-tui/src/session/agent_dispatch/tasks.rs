//! The /tasks pane's inbound records: the task list and one task's detail.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::tasks::BackgroundTaskDetail;
use houyicoder_protocol::frontend::tasks::BackgroundTaskSnapshot;

use crate::state::App;

impl App {
    /// Replace the session's task records with a fresh list. Replacing the
    /// whole set drops tasks a new session no longer holds, so the pane and
    /// the status bar count read one answer.
    pub(crate) fn apply_background_tasks(&mut self, tasks: Vec<BackgroundTaskSnapshot>) {
        self.sessions.current.capabilities.tasks.replace_all(tasks);
    }

    /// Fill the open detail with the record its read returned. A reply for
    /// an earlier open is ignored, so a slower read cannot overwrite a newer
    /// record or reopen a detail the user closed.
    pub(crate) fn apply_task_detail(&mut self, request: RequestId, detail: BackgroundTaskDetail) {
        self.ui.tasks_pane.apply_detail(request, detail);
    }
}
