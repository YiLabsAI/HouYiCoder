//! The /tasks pane actions: opening the pane, opening one task's detail,
//! stopping a task, and the reads that keep the open pane current.
//!
//! The host owns every background command. The pane asks again while it is
//! open because the host computes each run time at the moment it answers, so
//! a duration on screen is never the one carried by an old snapshot.

use std::time::Instant;

use houyicoder_protocol::frontend::tasks::BackgroundTaskId;

use crate::agent_message::ClientCommand;
use crate::state::{App, Pane};
use crate::view::tasks_pane::rows::{task_ids, task_rows};

impl App {
    /// The pane's rows in screen order, as task numbers.
    fn task_row_ids(&self) -> Vec<BackgroundTaskId> {
        task_ids(&task_rows(self.sessions.current.capabilities.tasks.iter()))
    }

    /// Open the pane on the list, its first row selected, and ask for the
    /// current set of tasks.
    pub(crate) fn open_tasks_pane(&mut self) {
        self.ui.navigation.pane = Pane::Tasks;
        self.ui.tasks_pane.close_detail();
        self.ui.tasks_pane.reset_selection();
        self.ui.tasks_pane.begin_list_read(Instant::now());
        self.refresh_query(|req_id| ClientCommand::BackgroundTasksQuery { req_id });
    }

    /// Close the pane from its list level, ending both read schedules so a
    /// closed pane costs no traffic.
    pub(crate) fn close_tasks_pane(&mut self) {
        self.ui.tasks_pane.close_detail();
        self.ui.tasks_pane.stop_list_read();
    }

    /// Move the list cursor over the rows the pane shows.
    pub(crate) fn move_tasks_cursor(&mut self, delta: i32) {
        self.ui.tasks_pane.move_cursor(delta, &self.task_row_ids());
    }

    /// Open the detail for the selected task and ask for its record. Without
    /// a connection there is no record to read, so the detail stays closed
    /// and the user is told rather than left on a read that never answers.
    pub(crate) fn open_task_detail(&mut self) {
        let Some(id) = self.ui.tasks_pane.target(&self.task_row_ids()) else {
            return;
        };
        let Some(req_id) = self.claim_request_id() else {
            if self.sessions.current.connection.is_none() {
                self.system_line("tasks: not connected");
            }
            return;
        };
        self.ui.tasks_pane.open_detail(id, req_id, Instant::now());
        self.enqueue_refresh(ClientCommand::BackgroundTaskDetailQuery { req_id, id });
    }

    /// Stop the selected task's command. The host kills the command's process
    /// group, so the processes it started end with it. A task that already
    /// ended has nothing to stop, and the user learns that rather than
    /// pressing a key that does nothing.
    pub(crate) fn kill_selected_task(&mut self) {
        let Some(id) = self.ui.tasks_pane.target(&self.task_row_ids()) else {
            return;
        };
        let running = self
            .sessions
            .current
            .capabilities
            .tasks
            .get(id)
            .is_some_and(|task| !task.status.is_terminal());
        if !running {
            self.system_line(format!("{}: not running", id.label()));
            return;
        }
        let Some(req_id) = self.claim_request_id() else {
            if self.sessions.current.connection.is_none() {
                self.system_line("tasks: not connected");
            }
            return;
        };
        if let Err(error) = self.enqueue(ClientCommand::KillBackgroundTask { req_id, id }) {
            self.system_line(Self::enqueue_failure_line("tasks", error));
        }
    }

    /// Run the pane's read schedules: the list once a second while the pane
    /// is open, and the open detail once a second until its task ends.
    pub(crate) fn tick_tasks(&mut self, now: Instant) {
        if self.ui.navigation.pane != Pane::Tasks {
            return;
        }
        if self.ui.tasks_pane.list_due(now) {
            self.ui.tasks_pane.mark_list_read(now);
            self.refresh_query(|req_id| ClientCommand::BackgroundTasksQuery { req_id });
        }
        if let Some(id) = self.ui.tasks_pane.detail_due(now)
            && let Some(req_id) = self.claim_request_id()
        {
            self.ui.tasks_pane.mark_detail_read(req_id, now);
            self.enqueue_refresh(ClientCommand::BackgroundTaskDetailQuery { req_id, id });
        }
    }
}
