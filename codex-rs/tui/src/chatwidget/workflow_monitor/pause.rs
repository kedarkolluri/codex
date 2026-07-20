//! Confirmed pause and exact-run resume controls for the workflow monitor.

use super::ChatWidget;
use super::WorkflowMonitor;
use super::bounded_inline_text;
use crate::app_event::AppEvent;
use crate::app_event::WorkflowRunControlTarget;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use codex_app_server_protocol::WorkflowPauseDisposition;
use codex_protocol::ThreadId;

const WORKFLOW_PAUSE_VIEW_ID: &str = "workflow-pause-confirmation";
const MAX_CONTROL_ERROR_CHARS: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkflowPauseRequestState {
    Idle,
    Pending { thread_id: ThreadId },
    Applied,
    AlreadyRequested,
    Failed(String),
}

impl WorkflowPauseRequestState {
    pub(super) fn can_request(&self) -> bool {
        matches!(self, Self::Idle | Self::Failed(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkflowResumeRequestState {
    Idle,
    Pending { thread_id: ThreadId },
    Succeeded { successor_run_id: String },
    Failed(String),
}

impl WorkflowResumeRequestState {
    fn can_request(&self) -> bool {
        matches!(self, Self::Idle | Self::Failed(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkflowPauseDialogTarget {
    control: WorkflowRunControlTarget,
    name: String,
}

impl WorkflowMonitor {
    fn selected_pause_target(&self) -> Option<WorkflowPauseDialogTarget> {
        let selected_run_id = self.selected_run_id.as_deref()?;
        let run = self
            .runs
            .iter()
            .find(|run| run.model.run_id == selected_run_id)?;
        if !run.is_effectively_running()
            || !run.pause_request.can_request()
            || !run.stop_request.can_request()
        {
            return None;
        }
        Some(WorkflowPauseDialogTarget {
            control: WorkflowRunControlTarget {
                thread_id: run.identity.thread_id?,
                run_id: run.model.run_id.clone(),
            },
            name: run.model.name.clone(),
        })
    }

    pub(super) fn selected_run_can_pause(&self) -> bool {
        self.selected_pause_target().is_some()
    }

    pub(super) fn selected_resume_target(&self) -> Option<WorkflowRunControlTarget> {
        let selected_run_id = self.selected_run_id.as_deref()?;
        let run = self
            .runs
            .iter()
            .find(|run| run.model.run_id == selected_run_id)?;
        if !run.is_effectively_paused() || !run.resume_request.can_request() {
            return None;
        }
        Some(WorkflowRunControlTarget {
            thread_id: run.identity.thread_id?,
            run_id: run.model.run_id.clone(),
        })
    }

    pub(super) fn selected_run_can_resume(&self) -> bool {
        self.selected_resume_target().is_some()
    }

    fn begin_pause_request(&mut self, target: &WorkflowRunControlTarget) -> bool {
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == target.run_id)
        else {
            return false;
        };
        if !run.is_effectively_running()
            || run.identity.thread_id != Some(target.thread_id)
            || !run.pause_request.can_request()
            || !run.stop_request.can_request()
        {
            return false;
        }
        run.pause_request = WorkflowPauseRequestState::Pending {
            thread_id: target.thread_id,
        };
        true
    }

    fn finish_pause_request(
        &mut self,
        target: &WorkflowRunControlTarget,
        result: &Result<WorkflowPauseDisposition, String>,
    ) -> Option<String> {
        let run = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == target.run_id)?;
        if run.pause_request
            != (WorkflowPauseRequestState::Pending {
                thread_id: target.thread_id,
            })
        {
            return None;
        }
        run.pause_request = match result {
            Ok(WorkflowPauseDisposition::Applied) => WorkflowPauseRequestState::Applied,
            Ok(WorkflowPauseDisposition::AlreadyRequested) => {
                WorkflowPauseRequestState::AlreadyRequested
            }
            Err(error) => WorkflowPauseRequestState::Failed(bounded_inline_text(
                error,
                MAX_CONTROL_ERROR_CHARS,
            )),
        };
        Some(run.model.name.clone())
    }

    fn begin_resume_request(&mut self, target: &WorkflowRunControlTarget) -> bool {
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == target.run_id)
        else {
            return false;
        };
        if !run.is_effectively_paused()
            || run.identity.thread_id != Some(target.thread_id)
            || !run.resume_request.can_request()
        {
            return false;
        }
        run.resume_request = WorkflowResumeRequestState::Pending {
            thread_id: target.thread_id,
        };
        true
    }

    fn finish_resume_request(
        &mut self,
        target: &WorkflowRunControlTarget,
        result: &Result<String, String>,
    ) -> Option<String> {
        let run = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == target.run_id)?;
        if run.resume_request
            != (WorkflowResumeRequestState::Pending {
                thread_id: target.thread_id,
            })
        {
            return None;
        }
        run.resume_request = match result {
            Ok(successor_run_id) => WorkflowResumeRequestState::Succeeded {
                successor_run_id: successor_run_id.clone(),
            },
            Err(error) => WorkflowResumeRequestState::Failed(bounded_inline_text(
                error,
                MAX_CONTROL_ERROR_CHARS,
            )),
        };
        Some(run.model.name.clone())
    }
}

impl ChatWidget {
    pub(crate) fn open_workflow_pause_confirmation(&mut self) -> bool {
        let Some(thread_id) = self.thread_id else {
            return false;
        };
        let Some(target) = self.workflow_monitor.selected_pause_target() else {
            return false;
        };
        if target.control.thread_id != thread_id {
            return false;
        }
        let request_target = target.control.clone();
        let pause_actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            tx.send(AppEvent::RequestWorkflowPause {
                target: request_target.clone(),
            });
        })];
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(WORKFLOW_PAUSE_VIEW_ID),
            title: Some("Pause workflow?".to_string()),
            subtitle: Some(format!("{} · {}", target.name, target.control.run_id)),
            items: vec![
                SelectionItem {
                    name: "Pause selected workflow".to_string(),
                    description: Some(
                        "Cancel active work after publishing its durable resume checkpoint."
                            .to_string(),
                    ),
                    actions: pause_actions,
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

    pub(crate) fn request_selected_workflow_resume(&mut self) -> bool {
        let Some(thread_id) = self.thread_id else {
            return false;
        };
        let Some(target) = self.workflow_monitor.selected_resume_target() else {
            return false;
        };
        if target.thread_id != thread_id {
            return false;
        }
        self.app_event_tx
            .send(AppEvent::RequestWorkflowResume { target });
        true
    }

    pub(crate) fn on_workflow_pause_requested(
        &mut self,
        target: &WorkflowRunControlTarget,
    ) -> bool {
        if self.thread_id != Some(target.thread_id) {
            return false;
        }
        if !self.workflow_monitor.begin_pause_request(target) {
            self.add_error_message(
                "That workflow run is no longer active or already has a control request."
                    .to_string(),
            );
            return false;
        }
        self.request_redraw();
        true
    }

    pub(crate) fn on_workflow_pause_finished(
        &mut self,
        target: WorkflowRunControlTarget,
        result: Result<WorkflowPauseDisposition, String>,
    ) {
        let Some(name) = self.workflow_monitor.finish_pause_request(&target, &result) else {
            return;
        };
        if self.thread_id != Some(target.thread_id) {
            return;
        }
        match result {
            Ok(WorkflowPauseDisposition::Applied) => self.add_info_message(
                format!("Paused workflow `{name}`."),
                Some(format!("Run {}", target.run_id)),
            ),
            Ok(WorkflowPauseDisposition::AlreadyRequested) => self.add_info_message(
                format!("Workflow `{name}` was already paused."),
                Some(format!("Run {}", target.run_id)),
            ),
            Err(error) => self.add_error_message(format!(
                "Failed to pause workflow `{name}`: {}",
                bounded_inline_text(&error, MAX_CONTROL_ERROR_CHARS)
            )),
        }
        self.request_redraw();
    }

    pub(crate) fn on_workflow_resume_requested(
        &mut self,
        target: &WorkflowRunControlTarget,
    ) -> bool {
        if self.thread_id != Some(target.thread_id) {
            return false;
        }
        if !self.workflow_monitor.begin_resume_request(target) {
            self.add_error_message(
                "That workflow run is no longer paused or already has a resume request."
                    .to_string(),
            );
            return false;
        }
        self.request_redraw();
        true
    }

    pub(crate) fn on_workflow_resume_finished(
        &mut self,
        target: WorkflowRunControlTarget,
        result: Result<String, String>,
    ) {
        let result = result.and_then(|run_id| {
            uuid::Uuid::parse_str(&run_id)
                .map(|run_id| run_id.to_string())
                .map_err(|_| "workflow resume returned an invalid successor".to_string())
        });
        let Some(name) = self
            .workflow_monitor
            .finish_resume_request(&target, &result)
        else {
            return;
        };
        if let Ok(successor_run_id) = &result {
            self.workflow_monitor
                .register_start_response(successor_run_id.clone(), name.clone());
        }
        if self.thread_id != Some(target.thread_id) {
            return;
        }
        match result {
            Ok(successor_run_id) => self.add_info_message(
                format!("Resumed workflow `{name}`."),
                Some(format!("Run {successor_run_id} · from {}", target.run_id)),
            ),
            Err(error) => self.add_error_message(format!(
                "Failed to resume workflow `{name}`: {}",
                bounded_inline_text(&error, MAX_CONTROL_ERROR_CHARS)
            )),
        }
        self.request_redraw();
    }
}
