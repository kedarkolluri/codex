//! Confirmed, run-targeted stop controls for the workflow monitor.

use super::ChatWidget;
use super::WorkflowMonitor;
use super::bounded_inline_text;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use codex_app_server_protocol::WorkflowStopDisposition;
use codex_protocol::ThreadId;

const WORKFLOW_STOP_VIEW_ID: &str = "workflow-stop-confirmation";
const MAX_STOP_ERROR_CHARS: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkflowStopRequestState {
    Idle,
    Pending { thread_id: ThreadId },
    Applied,
    AlreadyRequested,
    Failed(String),
}

impl WorkflowStopRequestState {
    pub(super) fn can_request(&self) -> bool {
        matches!(self, Self::Idle | Self::Failed(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkflowStopTarget {
    thread_id: ThreadId,
    run_id: String,
    name: String,
}

impl WorkflowMonitor {
    fn selected_stop_target(&self) -> Option<WorkflowStopTarget> {
        let selected_run_id = self.selected_run_id.as_deref()?;
        let run = self
            .runs
            .iter()
            .find(|run| run.model.run_id == selected_run_id)?;
        if !run.is_effectively_running()
            || !run.stop_request.can_request()
            || !run.pause_request.can_request()
        {
            return None;
        }
        Some(WorkflowStopTarget {
            thread_id: run.identity.thread_id?,
            run_id: run.model.run_id.clone(),
            name: run.model.name.clone(),
        })
    }

    pub(super) fn selected_run_can_stop(&self) -> bool {
        self.selected_stop_target().is_some()
    }

    fn begin_stop_request(&mut self, thread_id: ThreadId, run_id: &str) -> bool {
        let Some(run) = self.runs.iter_mut().find(|run| run.model.run_id == run_id) else {
            return false;
        };
        if !run.is_effectively_running()
            || run.identity.thread_id != Some(thread_id)
            || !run.stop_request.can_request()
            || !run.pause_request.can_request()
        {
            return false;
        }
        run.stop_request = WorkflowStopRequestState::Pending { thread_id };
        true
    }

    fn finish_stop_request(
        &mut self,
        thread_id: ThreadId,
        run_id: &str,
        result: &Result<WorkflowStopDisposition, String>,
    ) -> Option<String> {
        let run = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == run_id)?;
        if run.stop_request != (WorkflowStopRequestState::Pending { thread_id }) {
            return None;
        }
        run.stop_request = match result {
            Ok(WorkflowStopDisposition::Applied) => WorkflowStopRequestState::Applied,
            Ok(WorkflowStopDisposition::AlreadyRequested) => {
                WorkflowStopRequestState::AlreadyRequested
            }
            Err(error) => {
                WorkflowStopRequestState::Failed(bounded_inline_text(error, MAX_STOP_ERROR_CHARS))
            }
        };
        Some(run.model.name.clone())
    }
}

impl ChatWidget {
    pub(crate) fn open_workflow_stop_confirmation(&mut self) -> bool {
        let Some(thread_id) = self.thread_id else {
            return false;
        };
        let Some(target) = self.workflow_monitor.selected_stop_target() else {
            return false;
        };
        if target.thread_id != thread_id {
            return false;
        }
        let run_id = target.run_id.clone();
        let stop_actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            tx.send(AppEvent::RequestWorkflowStop {
                thread_id,
                run_id: run_id.clone(),
            });
        })];
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(WORKFLOW_STOP_VIEW_ID),
            title: Some("Stop workflow?".to_string()),
            subtitle: Some(format!("{} · {}", target.name, target.run_id)),
            items: vec![
                SelectionItem {
                    name: "Stop selected workflow".to_string(),
                    description: Some(
                        "Cancel this run and wait for its agents and resources to stop."
                            .to_string(),
                    ),
                    actions: stop_actions,
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    description: Some("Keep the workflow running.".to_string()),
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            footer_hint: Some(standard_popup_hint_line()),
            initial_selected_idx: Some(1),
            ..Default::default()
        });
        self.request_redraw();
        true
    }

    pub(crate) fn on_workflow_stop_requested(&mut self, thread_id: ThreadId, run_id: &str) -> bool {
        if self.thread_id != Some(thread_id) {
            return false;
        }
        if !self.workflow_monitor.begin_stop_request(thread_id, run_id) {
            self.add_error_message(
                "That workflow run is no longer active or already has a stop request.".to_string(),
            );
            return false;
        }
        self.request_redraw();
        true
    }

    pub(crate) fn on_workflow_stop_finished(
        &mut self,
        thread_id: ThreadId,
        run_id: &str,
        result: Result<WorkflowStopDisposition, String>,
    ) {
        let Some(name) = self
            .workflow_monitor
            .finish_stop_request(thread_id, run_id, &result)
        else {
            return;
        };
        if self.thread_id != Some(thread_id) {
            return;
        }
        match result {
            Ok(WorkflowStopDisposition::Applied) => self.add_info_message(
                format!("Stopped workflow `{name}`."),
                Some(format!("Run {run_id}")),
            ),
            Ok(WorkflowStopDisposition::AlreadyRequested) => self.add_info_message(
                format!("Workflow `{name}` was already stopped."),
                Some(format!("Run {run_id}")),
            ),
            Err(error) => {
                let error = bounded_inline_text(&error, MAX_STOP_ERROR_CHARS);
                self.add_error_message(format!("Failed to stop workflow `{name}`: {error}"));
            }
        }
        self.request_redraw();
    }
}
