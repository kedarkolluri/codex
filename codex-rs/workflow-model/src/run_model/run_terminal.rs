use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;

use super::agent_lifecycle::validate_terminal_status;
use super::*;

impl WorkflowRunModel {
    pub(super) fn reduce_run_end(
        &mut self,
        event: &WorkflowRunEndEvent,
    ) -> Result<(), WorkflowModelError> {
        if let Some(node_id) = self
            .topology
            .values()
            .find_map(|node| (node.state() == WorkflowNodeState::Active).then_some(node.id()))
        {
            return Err(WorkflowModelError::ActiveTopologyAtRunEnd { node_id });
        }
        validate_terminal_status(/*node_id*/ None, &event.status)?;
        validate_terminal_reason(&event.status, event.terminal_reason)?;
        if event.spent < 0 || event.total.is_some_and(|total| total < 0) {
            return Err(WorkflowModelError::NegativeBudget {
                spent: event.spent,
                total: event.total,
            });
        }
        let active_phase_position = if let Some(phase_index) = self.active_phase_index {
            let position =
                usize::try_from(phase_index).map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
            self.phases
                .get(position)
                .ok_or(WorkflowModelError::PhaseNotActive { phase_index })?;
            Some(position)
        } else {
            None
        };

        if let Some(position) = active_phase_position {
            self.phases[position].state = WorkflowPhaseState::Completed;
        }
        self.active_phase_index = None;
        self.state = WorkflowRunState::Completed;
        self.status.clone_from(&event.status);
        self.terminal_reason = event.terminal_reason;
        self.budget = Some(WorkflowBudgetSummary {
            spent: event.spent,
            total: event.total,
        });
        Ok(())
    }
}

fn validate_terminal_reason(
    status: &AgentStatus,
    terminal_reason: Option<WorkflowRunTerminalReason>,
) -> Result<(), WorkflowModelError> {
    let Some(terminal_reason) = terminal_reason else {
        // Older persisted workflow events predate the exact reason field.
        return Ok(());
    };
    let compatible = match terminal_reason {
        WorkflowRunTerminalReason::Completed => matches!(status, AgentStatus::Completed(_)),
        WorkflowRunTerminalReason::Failed => matches!(status, AgentStatus::Errored(_)),
        WorkflowRunTerminalReason::Interrupted => matches!(status, AgentStatus::Interrupted),
        WorkflowRunTerminalReason::Stopped => matches!(status, AgentStatus::Shutdown),
        WorkflowRunTerminalReason::Paused => matches!(status, AgentStatus::Interrupted),
    };
    if compatible {
        Ok(())
    } else {
        Err(WorkflowModelError::TerminalReasonMismatch {
            status: status.clone(),
            terminal_reason,
        })
    }
}
