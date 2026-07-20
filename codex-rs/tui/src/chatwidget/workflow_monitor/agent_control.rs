//! Exact-attempt skip and retry controls for selected live workflow agents.

use super::ChatWidget;
use super::WorkflowMonitor;
use super::WorkflowTopologyNode;
use super::bounded_inline_text;
use crate::app_event::AppEvent;
use crate::app_event::WorkflowAgentControlRequest;
use crate::app_event::WorkflowAgentControlTarget;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use codex_app_server_protocol::WorkflowAgentControlAction;
use codex_app_server_protocol::WorkflowAgentControlResponse;
use codex_core_workflows::WorkflowNodeState;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;

const WORKFLOW_AGENT_SKIP_VIEW_ID: &str = "workflow-agent-skip-confirmation";
const WORKFLOW_AGENT_RETRY_VIEW_ID: &str = "workflow-agent-retry-confirmation";
const MAX_CONTROL_ERROR_CHARS: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkflowAgentControlRequestState {
    Pending(WorkflowAgentControlRequest),
    Skipped(WorkflowAgentControlTarget),
    RetryScheduled {
        target: WorkflowAgentControlTarget,
        attempt: u32,
    },
    RetryLimitReached(WorkflowAgentControlTarget),
    Failed {
        target: WorkflowAgentControlTarget,
        error: String,
    },
}

impl WorkflowAgentControlRequestState {
    fn blocks_attempt(&self, current_attempt: u32) -> bool {
        match self {
            Self::Pending(_) => true,
            Self::Skipped(target) | Self::RetryLimitReached(target) => {
                target.attempt == current_attempt
            }
            Self::RetryScheduled { attempt, .. } => current_attempt != *attempt,
            Self::Failed { .. } => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkflowAgentControlDialogTarget {
    target: WorkflowAgentControlTarget,
    label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkflowAgentControlOutcome {
    Skipped,
    RetryScheduled { attempt: u32 },
    RetryLimitReached,
    Failed(String),
}

impl WorkflowMonitor {
    fn selected_agent_control_target(&self) -> Option<WorkflowAgentControlDialogTarget> {
        let selection = self.selection.as_ref()?;
        let run = self
            .runs
            .iter()
            .find(|run| run.model.run_id == selection.run_id)?;
        if !run.is_effectively_running()
            || !run.stop_request.can_request()
            || !run.pause_request.can_request()
        {
            return None;
        }
        let WorkflowTopologyNode::Agent(agent) = run.model.topology.get(&selection.node_id)? else {
            return None;
        };
        if agent.state != WorkflowNodeState::Active
            || agent.status != AgentStatus::Running
            || !agent_has_valid_binding(agent)
            || run
                .agent_control_requests
                .get(&selection.node_id)
                .is_some_and(|state| state.blocks_attempt(agent.attempt))
        {
            return None;
        }
        Some(WorkflowAgentControlDialogTarget {
            target: WorkflowAgentControlTarget {
                thread_id: run.identity.thread_id?,
                run_id: run.model.run_id.clone(),
                node_id: agent.id,
                attempt: agent.attempt,
            },
            label: agent.label.clone(),
        })
    }

    pub(super) fn selected_agent_can_control(&self) -> bool {
        self.selected_agent_control_target().is_some()
    }

    fn begin_agent_control_request(&mut self, request: &WorkflowAgentControlRequest) -> bool {
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == request.target.run_id)
        else {
            return false;
        };
        if !run.is_effectively_running()
            || run.identity.thread_id != Some(request.target.thread_id)
            || !run.stop_request.can_request()
            || !run.pause_request.can_request()
        {
            return false;
        }
        let Some(WorkflowTopologyNode::Agent(agent)) =
            run.model.topology.get(&request.target.node_id)
        else {
            return false;
        };
        if agent.state != WorkflowNodeState::Active
            || agent.status != AgentStatus::Running
            || !agent_has_valid_binding(agent)
            || agent.attempt != request.target.attempt
            || run
                .agent_control_requests
                .get(&request.target.node_id)
                .is_some_and(|state| state.blocks_attempt(agent.attempt))
        {
            return false;
        }
        run.agent_control_requests.insert(
            request.target.node_id,
            WorkflowAgentControlRequestState::Pending(request.clone()),
        );
        true
    }

    fn finish_agent_control_request(
        &mut self,
        request: &WorkflowAgentControlRequest,
        result: &Result<WorkflowAgentControlResponse, String>,
    ) -> Option<(String, WorkflowAgentControlOutcome)> {
        let run = self
            .runs
            .iter_mut()
            .find(|run| run.model.run_id == request.target.run_id)?;
        if run.agent_control_requests.get(&request.target.node_id)
            != Some(&WorkflowAgentControlRequestState::Pending(request.clone()))
        {
            return None;
        }
        let label = match run.model.topology.get(&request.target.node_id) {
            Some(WorkflowTopologyNode::Agent(agent)) => agent.label.clone(),
            Some(WorkflowTopologyNode::Group(_)) | None => return None,
        };
        let (state, outcome) = match (request.action, result) {
            (WorkflowAgentControlAction::Skip, Ok(WorkflowAgentControlResponse::Skipped)) => (
                WorkflowAgentControlRequestState::Skipped(request.target.clone()),
                WorkflowAgentControlOutcome::Skipped,
            ),
            (
                WorkflowAgentControlAction::Retry,
                Ok(WorkflowAgentControlResponse::RetryScheduled { attempt }),
            ) if Some(*attempt) == request.target.attempt.checked_add(1) => (
                WorkflowAgentControlRequestState::RetryScheduled {
                    target: request.target.clone(),
                    attempt: *attempt,
                },
                WorkflowAgentControlOutcome::RetryScheduled { attempt: *attempt },
            ),
            (
                WorkflowAgentControlAction::Retry,
                Ok(WorkflowAgentControlResponse::RetryLimitReached),
            ) => (
                WorkflowAgentControlRequestState::RetryLimitReached(request.target.clone()),
                WorkflowAgentControlOutcome::RetryLimitReached,
            ),
            (_, Err(error)) => {
                let error = bounded_inline_text(error, MAX_CONTROL_ERROR_CHARS);
                (
                    WorkflowAgentControlRequestState::Failed {
                        target: request.target.clone(),
                        error: error.clone(),
                    },
                    WorkflowAgentControlOutcome::Failed(error),
                )
            }
            (WorkflowAgentControlAction::Skip | WorkflowAgentControlAction::Retry, Ok(_)) => {
                let error = "unexpected workflow agent control response".to_string();
                (
                    WorkflowAgentControlRequestState::Failed {
                        target: request.target.clone(),
                        error: error.clone(),
                    },
                    WorkflowAgentControlOutcome::Failed(error),
                )
            }
        };
        run.agent_control_requests
            .insert(request.target.node_id, state);
        Some((label, outcome))
    }
}

fn agent_has_valid_binding(agent: &codex_core_workflows::WorkflowAgent) -> bool {
    agent
        .child_thread_id
        .as_deref()
        .and_then(|child_thread_id| ThreadId::from_string(child_thread_id).ok())
        .is_some()
}

impl ChatWidget {
    pub(crate) fn open_workflow_agent_control_confirmation(
        &mut self,
        action: WorkflowAgentControlAction,
    ) -> bool {
        let Some(thread_id) = self.thread_id else {
            return false;
        };
        let Some(dialog) = self.workflow_monitor.selected_agent_control_target() else {
            return false;
        };
        if dialog.target.thread_id != thread_id {
            return false;
        }
        let request = WorkflowAgentControlRequest {
            target: dialog.target.clone(),
            action,
        };
        let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            tx.send(AppEvent::RequestWorkflowAgentControl {
                request: request.clone(),
            });
        })];
        let (view_id, title, action_name, description, cancel_description) = match action {
            WorkflowAgentControlAction::Skip => (
                WORKFLOW_AGENT_SKIP_VIEW_ID,
                "Skip workflow agent?",
                "Skip selected attempt",
                "Cancel this exact attempt and settle its logical call to null.",
                "Keep this agent attempt running.",
            ),
            WorkflowAgentControlAction::Retry => (
                WORKFLOW_AGENT_RETRY_VIEW_ID,
                "Retry workflow agent?",
                "Retry selected attempt",
                "Cancel this exact attempt and schedule a fresh child for the same logical call.",
                "Keep this agent attempt running.",
            ),
        };
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(view_id),
            title: Some(title.to_string()),
            subtitle: Some(format!(
                "{} · run {} · attempt {}",
                dialog.label,
                dialog.target.run_id,
                dialog.target.attempt.saturating_add(1)
            )),
            items: vec![
                SelectionItem {
                    name: action_name.to_string(),
                    description: Some(description.to_string()),
                    actions,
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    description: Some(cancel_description.to_string()),
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

    pub(crate) fn on_workflow_agent_control_requested(
        &mut self,
        request: &WorkflowAgentControlRequest,
    ) -> bool {
        if self.thread_id != Some(request.target.thread_id) {
            return false;
        }
        if !self.workflow_monitor.begin_agent_control_request(request) {
            self.add_error_message(
                "That exact workflow agent attempt is no longer available.".to_string(),
            );
            return false;
        }
        self.request_redraw();
        true
    }

    pub(crate) fn on_workflow_agent_control_finished(
        &mut self,
        request: WorkflowAgentControlRequest,
        result: Result<WorkflowAgentControlResponse, String>,
    ) {
        let Some((label, outcome)) = self
            .workflow_monitor
            .finish_agent_control_request(&request, &result)
        else {
            return;
        };
        if self.thread_id != Some(request.target.thread_id) {
            return;
        }
        match outcome {
            WorkflowAgentControlOutcome::Skipped => self.add_info_message(
                format!("Skipped workflow agent `{label}`."),
                Some(format!(
                    "Run {} · attempt {}",
                    request.target.run_id,
                    request.target.attempt.saturating_add(1)
                )),
            ),
            WorkflowAgentControlOutcome::RetryScheduled { attempt } => self.add_info_message(
                format!("Retrying workflow agent `{label}`."),
                Some(format!(
                    "Run {} · attempt {}",
                    request.target.run_id,
                    attempt.saturating_add(1)
                )),
            ),
            WorkflowAgentControlOutcome::RetryLimitReached => {
                self.add_error_message(format!("Workflow agent `{label}` reached its retry limit."))
            }
            WorkflowAgentControlOutcome::Failed(error) => self.add_error_message(format!(
                "Failed to control workflow agent `{label}`: {}",
                bounded_inline_text(&error, MAX_CONTROL_ERROR_CHARS)
            )),
        }
        self.request_redraw();
    }
}
