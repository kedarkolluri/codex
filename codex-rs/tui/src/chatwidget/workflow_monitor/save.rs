//! Exact-script save controls for explicitly selected completed workflow runs.

use super::ChatWidget;
use super::WorkflowMonitor;
use super::WorkflowRunState;
use super::bounded_inline_text;
use crate::app_event::AppEvent;
use crate::app_event::WorkflowSaveIntent;
use crate::app_event::WorkflowSaveRequest;
use crate::app_event::WorkflowSaveRunTarget;
use crate::app_event::WorkflowSaveTarget;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use codex_app_server_protocol::WorkflowRunStatus;
use codex_app_server_protocol::WorkflowSaveDisposition;
use codex_app_server_protocol::WorkflowSaveScope;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowRunTerminalReason;

const WORKFLOW_SAVE_NAME_VIEW_ID: &str = "workflow-save-name-confirmation";
const WORKFLOW_SAVE_SCOPE_VIEW_ID: &str = "workflow-save-scope";
const WORKFLOW_SAVE_OVERWRITE_VIEW_ID: &str = "workflow-save-overwrite-confirmation";
pub(super) const MAX_SAVE_ERROR_CHARS: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkflowSaveRequestState {
    Idle,
    Pending(WorkflowSaveRequest),
    Conflict(WorkflowSaveTarget),
    Created(WorkflowSaveTarget),
    Overwritten(WorkflowSaveTarget),
    Failed {
        target: WorkflowSaveTarget,
        error: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkflowSaveDialog {
    Start(WorkflowSaveRunTarget),
    ConfirmOverwrite(WorkflowSaveTarget),
}

impl WorkflowMonitor {
    pub(super) fn selected_run_can_save(&self) -> bool {
        self.selected_save_run().is_some_and(|run| {
            run.effective_state() == WorkflowRunState::Completed
                && run_is_saveable(run)
                && run.identity.thread_id.is_some()
                && run.identity.durable_name.is_some()
                && !matches!(run.save_request, WorkflowSaveRequestState::Pending(_))
        })
    }

    fn selected_save_dialog(&self, thread_id: ThreadId) -> Option<WorkflowSaveDialog> {
        let run = self.selected_save_run()?;
        if run.effective_state() != WorkflowRunState::Completed || !run_is_saveable(run) {
            return None;
        }
        if run.identity.thread_id != Some(thread_id) {
            return None;
        }
        let target = WorkflowSaveRunTarget {
            thread_id,
            run_id: run.model.run_id.clone(),
            name: run.identity.durable_name.clone()?,
        };
        match &run.save_request {
            WorkflowSaveRequestState::Pending(_) => None,
            WorkflowSaveRequestState::Conflict(conflict) if conflict.run == target => {
                Some(WorkflowSaveDialog::ConfirmOverwrite(conflict.clone()))
            }
            WorkflowSaveRequestState::Idle
            | WorkflowSaveRequestState::Created(_)
            | WorkflowSaveRequestState::Overwritten(_)
            | WorkflowSaveRequestState::Failed { .. } => Some(WorkflowSaveDialog::Start(target)),
            WorkflowSaveRequestState::Conflict(_) => None,
        }
    }

    fn selected_save_run(&self) -> Option<&super::MonitoredRun> {
        let selected_run_id = self.selected_run_id.as_deref()?;
        self.runs
            .iter()
            .find(|run| run.model.run_id == selected_run_id)
    }

    fn can_prepare_create(&self, target: &WorkflowSaveRunTarget) -> bool {
        let Some(run) = self.selected_save_run() else {
            return false;
        };
        run.effective_state() == WorkflowRunState::Completed
            && run_is_saveable(run)
            && run.model.run_id == target.run_id
            && run.identity.thread_id == Some(target.thread_id)
            && run.identity.durable_name.as_deref() == Some(target.name.as_str())
            && matches!(
                run.save_request,
                WorkflowSaveRequestState::Idle
                    | WorkflowSaveRequestState::Created(_)
                    | WorkflowSaveRequestState::Overwritten(_)
                    | WorkflowSaveRequestState::Failed { .. }
            )
    }

    fn can_confirm_overwrite(&self, target: &WorkflowSaveTarget) -> bool {
        let Some(run) = self.selected_save_run() else {
            return false;
        };
        run.effective_state() == WorkflowRunState::Completed
            && run_is_saveable(run)
            && run.model.run_id == target.run.run_id
            && run.identity.thread_id == Some(target.run.thread_id)
            && run.identity.durable_name.as_deref() == Some(target.run.name.as_str())
            && run.save_request == WorkflowSaveRequestState::Conflict(target.clone())
    }

    fn begin_save_request(&mut self, request: &WorkflowSaveRequest) -> bool {
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == request.target.run.run_id)
        else {
            return false;
        };
        if run.effective_state() != WorkflowRunState::Completed
            || !run_is_saveable(run)
            || run.identity.thread_id != Some(request.target.run.thread_id)
            || run.identity.durable_name.as_deref() != Some(request.target.run.name.as_str())
        {
            return false;
        }
        let allowed = match request.intent {
            WorkflowSaveIntent::Create => matches!(
                run.save_request,
                WorkflowSaveRequestState::Idle
                    | WorkflowSaveRequestState::Created(_)
                    | WorkflowSaveRequestState::Overwritten(_)
                    | WorkflowSaveRequestState::Failed { .. }
            ),
            WorkflowSaveIntent::Overwrite => {
                run.save_request == WorkflowSaveRequestState::Conflict(request.target.clone())
            }
        };
        if !allowed {
            return false;
        }
        run.save_request = WorkflowSaveRequestState::Pending(request.clone());
        true
    }

    fn finish_save_request(
        &mut self,
        request: &WorkflowSaveRequest,
        result: &Result<WorkflowSaveDisposition, String>,
    ) -> Option<WorkflowSaveTarget> {
        let run = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == request.target.run.run_id)?;
        if run.save_request != WorkflowSaveRequestState::Pending(request.clone()) {
            return None;
        }
        let target = request.target.clone();
        run.save_request = match result {
            Ok(WorkflowSaveDisposition::Created) => {
                WorkflowSaveRequestState::Created(target.clone())
            }
            Ok(WorkflowSaveDisposition::Overwritten) => {
                WorkflowSaveRequestState::Overwritten(target.clone())
            }
            Ok(WorkflowSaveDisposition::Conflict) => {
                WorkflowSaveRequestState::Conflict(target.clone())
            }
            Err(error) => WorkflowSaveRequestState::Failed {
                target: target.clone(),
                error: bounded_inline_text(error, MAX_SAVE_ERROR_CHARS),
            },
        };
        Some(target)
    }

    fn cancel_save_conflict(&mut self, target: &WorkflowSaveTarget) -> bool {
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == target.run.run_id)
        else {
            return false;
        };
        if run.save_request != WorkflowSaveRequestState::Conflict(target.clone()) {
            return false;
        }
        run.save_request = WorkflowSaveRequestState::Idle;
        true
    }
}

fn run_is_saveable(run: &super::MonitoredRun) -> bool {
    match run.reconciled_status {
        Some(WorkflowRunStatus::Completed) => return true,
        Some(
            WorkflowRunStatus::Running
            | WorkflowRunStatus::Stopped
            | WorkflowRunStatus::Paused
            | WorkflowRunStatus::Failed
            | WorkflowRunStatus::Unknown,
        ) => return false,
        None => {}
    }
    match run.model.terminal_reason {
        Some(WorkflowRunTerminalReason::Completed) => true,
        Some(
            WorkflowRunTerminalReason::Failed
            | WorkflowRunTerminalReason::Interrupted
            | WorkflowRunTerminalReason::Stopped
            | WorkflowRunTerminalReason::Paused,
        ) => false,
        None => matches!(run.model.status, AgentStatus::Completed(_)),
    }
}

impl ChatWidget {
    pub(crate) fn open_workflow_save_dialog(&mut self) -> bool {
        let Some(thread_id) = self.thread_id else {
            return false;
        };
        let Some(dialog) = self.workflow_monitor.selected_save_dialog(thread_id) else {
            return false;
        };
        match dialog {
            WorkflowSaveDialog::Start(target) => self.open_workflow_save_name_confirmation(target),
            WorkflowSaveDialog::ConfirmOverwrite(target) => {
                self.open_workflow_save_overwrite_confirmation(target)
            }
        }
    }

    fn open_workflow_save_name_confirmation(&mut self, target: WorkflowSaveRunTarget) -> bool {
        let continue_target = target.clone();
        let continue_actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            tx.send(AppEvent::OpenWorkflowSaveScope {
                target: continue_target.clone(),
            });
        })];
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(WORKFLOW_SAVE_NAME_VIEW_ID),
            title: Some("Save completed workflow?".to_string()),
            subtitle: Some(format!("Exact durable name: {}", target.name)),
            items: vec![
                SelectionItem {
                    name: "Continue with this exact name".to_string(),
                    description: Some(
                        "The durable script is saved byte-for-byte; its name cannot be edited."
                            .to_string(),
                    ),
                    actions: continue_actions,
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    description: Some("Do not save this workflow.".to_string()),
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            footer_hint: Some(standard_popup_hint_line()),
            initial_selected_idx: Some(0),
            ..Default::default()
        });
        self.request_redraw();
        true
    }

    pub(crate) fn open_workflow_save_scope_picker(
        &mut self,
        target: WorkflowSaveRunTarget,
    ) -> bool {
        if self.thread_id != Some(target.thread_id)
            || !self.workflow_monitor.can_prepare_create(&target)
        {
            return false;
        }
        let personal_target = WorkflowSaveTarget {
            run: target.clone(),
            scope: WorkflowSaveScope::Personal,
        };
        let project_target = WorkflowSaveTarget {
            run: target.clone(),
            scope: WorkflowSaveScope::Project,
        };
        let personal_actions = create_save_actions(personal_target);
        let project_actions = create_save_actions(project_target);
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(WORKFLOW_SAVE_SCOPE_VIEW_ID),
            title: Some(format!("Save workflow `{}`", target.name)),
            subtitle: Some("Choose where to save the exact durable script.".to_string()),
            items: vec![
                SelectionItem {
                    name: "Personal".to_string(),
                    description: Some(
                        "Save to your private .agents workflow registry.".to_string(),
                    ),
                    actions: personal_actions,
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Project".to_string(),
                    description: Some(
                        "Scripts can contain secrets. Project files may be committed; review before commit."
                            .to_string(),
                    ),
                    actions: project_actions,
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    description: Some("Do not save this workflow.".to_string()),
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            footer_hint: Some(standard_popup_hint_line()),
            initial_selected_idx: Some(0),
            ..Default::default()
        });
        self.request_redraw();
        true
    }

    fn open_workflow_save_overwrite_confirmation(&mut self, target: WorkflowSaveTarget) -> bool {
        if self.thread_id != Some(target.run.thread_id)
            || !self.bottom_pane.no_modal_or_popup_active()
            || !self.workflow_monitor.can_confirm_overwrite(&target)
        {
            return false;
        }
        let overwrite_request = WorkflowSaveRequest {
            target: target.clone(),
            intent: WorkflowSaveIntent::Overwrite,
        };
        let overwrite_actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            tx.send(AppEvent::RequestWorkflowSave {
                request: overwrite_request.clone(),
            });
        })];
        let cancel_target = target.clone();
        let cancel_actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            tx.send(AppEvent::CancelWorkflowSaveConflict {
                target: cancel_target.clone(),
            });
        })];
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(WORKFLOW_SAVE_OVERWRITE_VIEW_ID),
            title: Some("Overwrite saved workflow?".to_string()),
            subtitle: Some(format!(
                "Exact name: {} · {} scope",
                target.run.name,
                workflow_save_scope_label(target.scope)
            )),
            items: vec![
                SelectionItem {
                    name: "Overwrite existing workflow".to_string(),
                    description: Some(
                        "Replace its script with the exact durable run script.".to_string(),
                    ),
                    actions: overwrite_actions,
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    description: Some("Leave the existing saved workflow unchanged.".to_string()),
                    actions: cancel_actions,
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

    pub(crate) fn on_workflow_save_requested(&mut self, request: &WorkflowSaveRequest) -> bool {
        if self.thread_id != Some(request.target.run.thread_id) {
            return false;
        }
        if !self.workflow_monitor.begin_save_request(request) {
            self.add_error_message(
                "That workflow run cannot be saved or already has a save request.".to_string(),
            );
            return false;
        }
        self.request_redraw();
        true
    }

    pub(crate) fn on_workflow_save_finished(
        &mut self,
        request: WorkflowSaveRequest,
        result: Result<WorkflowSaveDisposition, String>,
    ) {
        let Some(target) = self.workflow_monitor.finish_save_request(&request, &result) else {
            return;
        };
        if self.thread_id != Some(request.target.run.thread_id) {
            return;
        }
        let scope = workflow_save_scope_label(target.scope);
        match result {
            Ok(WorkflowSaveDisposition::Created) => self.add_info_message(
                format!(
                    "Saved exact workflow `{}` to {scope} workflows.",
                    target.run.name
                ),
                /*hint*/ None,
            ),
            Ok(WorkflowSaveDisposition::Overwritten) => self.add_info_message(
                format!(
                    "Overwrote {scope} workflow `{}` with the exact durable script.",
                    target.run.name
                ),
                /*hint*/ None,
            ),
            Ok(WorkflowSaveDisposition::Conflict) => {
                self.add_info_message(
                    format!(
                        "A {scope} workflow named `{}` already exists.",
                        target.run.name
                    ),
                    Some(
                        "Confirm overwrite or cancel; the existing script is unchanged."
                            .to_string(),
                    ),
                );
                self.open_workflow_save_overwrite_confirmation(target);
            }
            Err(error) => {
                let error = bounded_inline_text(&error, MAX_SAVE_ERROR_CHARS);
                self.add_error_message(format!(
                    "Failed to save workflow `{}`: {error}",
                    target.run.name
                ));
            }
        }
        self.request_redraw();
    }

    pub(crate) fn cancel_workflow_save_conflict(&mut self, target: WorkflowSaveTarget) {
        if self.thread_id == Some(target.run.thread_id)
            && self.workflow_monitor.cancel_save_conflict(&target)
        {
            self.request_redraw();
        }
    }
}

fn create_save_actions(target: WorkflowSaveTarget) -> Vec<SelectionAction> {
    let request = WorkflowSaveRequest {
        target,
        intent: WorkflowSaveIntent::Create,
    };
    vec![Box::new(move |tx| {
        tx.send(AppEvent::RequestWorkflowSave {
            request: request.clone(),
        });
    })]
}

pub(super) fn workflow_save_scope_label(scope: WorkflowSaveScope) -> &'static str {
    match scope {
        WorkflowSaveScope::Project => "project",
        WorkflowSaveScope::Personal => "personal",
    }
}
