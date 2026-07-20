use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowEvent;

use super::WorkflowAgent;
use super::WorkflowModelError;
use super::WorkflowPhase;
use super::WorkflowRunModel;
use super::WorkflowTopologyNode;

impl WorkflowRunModel {
    pub(super) fn validate_run_id(&self, event: &WorkflowEvent) -> Result<(), WorkflowModelError> {
        let actual = match event {
            WorkflowEvent::RunBegin(event) => &event.run_id,
            WorkflowEvent::RunEnd(event) => &event.run_id,
            WorkflowEvent::PhaseBegin(event) => &event.run_id,
            WorkflowEvent::PhaseEnd(event) => &event.run_id,
            WorkflowEvent::GroupBegin(event) => &event.run_id,
            WorkflowEvent::GroupEnd(event) => &event.run_id,
            WorkflowEvent::AgentBegin(event) => &event.run_id,
            WorkflowEvent::AgentBound(event) => &event.run_id,
            WorkflowEvent::AgentUpdated(event) => &event.run_id,
            WorkflowEvent::AgentEnd(event) => &event.run_id,
            WorkflowEvent::Log(event) => &event.run_id,
        };
        if actual == &self.run_id {
            Ok(())
        } else {
            Err(WorkflowModelError::RunIdMismatch {
                expected: self.run_id.clone(),
                actual: actual.clone(),
            })
        }
    }

    pub(super) fn require_active_phase(&self) -> Result<u64, WorkflowModelError> {
        self.active_phase_index
            .ok_or(WorkflowModelError::NoActivePhase)
    }

    pub(super) fn phase(&self, phase_index: u64) -> Result<&WorkflowPhase, WorkflowModelError> {
        let index =
            usize::try_from(phase_index).map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
        self.phases
            .get(index)
            .ok_or(WorkflowModelError::UnknownPhase { phase_index })
    }

    pub(super) fn phase_mut(
        &mut self,
        phase_index: u64,
    ) -> Result<&mut WorkflowPhase, WorkflowModelError> {
        let index =
            usize::try_from(phase_index).map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
        self.phases
            .get_mut(index)
            .ok_or(WorkflowModelError::UnknownPhase { phase_index })
    }

    pub(super) fn agent_mut(
        &mut self,
        node_id: u64,
    ) -> Result<&mut WorkflowAgent, WorkflowModelError> {
        let node = self
            .topology
            .get_mut(&node_id)
            .ok_or(WorkflowModelError::UnknownAgent { node_id })?;
        match node {
            WorkflowTopologyNode::Agent(agent) => Ok(agent),
            WorkflowTopologyNode::Group(_) => Err(WorkflowModelError::TopologyKindMismatch {
                node_id,
                expected: "agent",
            }),
        }
    }
}

pub(super) fn validate_terminal_status(
    node_id: Option<u64>,
    status: &AgentStatus,
) -> Result<(), WorkflowModelError> {
    if matches!(status, AgentStatus::PendingInit | AgentStatus::Running) {
        Err(WorkflowModelError::NonTerminalStatus {
            node_id,
            status: status.clone(),
        })
    } else {
        Ok(())
    }
}

pub(super) fn validate_counter_progress(
    node_id: u64,
    previous_usage: &TokenUsage,
    previous_tool_calls: u64,
    previous_duration_ms: u64,
    next_usage: &TokenUsage,
    next_tool_calls: u64,
    next_duration_ms: u64,
) -> Result<(), WorkflowModelError> {
    let counters = [
        (
            "input_tokens",
            previous_usage.input_tokens,
            next_usage.input_tokens,
        ),
        (
            "cached_input_tokens",
            previous_usage.cached_input_tokens,
            next_usage.cached_input_tokens,
        ),
        (
            "output_tokens",
            previous_usage.output_tokens,
            next_usage.output_tokens,
        ),
        (
            "reasoning_output_tokens",
            previous_usage.reasoning_output_tokens,
            next_usage.reasoning_output_tokens,
        ),
        (
            "total_tokens",
            previous_usage.total_tokens,
            next_usage.total_tokens,
        ),
        (
            "tool_call_count",
            i64::try_from(previous_tool_calls).unwrap_or(i64::MAX),
            i64::try_from(next_tool_calls).unwrap_or(i64::MAX),
        ),
    ];
    if let Some((field, previous, next)) = counters
        .into_iter()
        .find(|(_, previous, next)| next < previous)
    {
        Err(WorkflowModelError::CounterRegression {
            node_id,
            field,
            previous,
            next,
        })
    } else if next_duration_ms < previous_duration_ms {
        Err(WorkflowModelError::CounterRegression {
            node_id,
            field: "duration_ms",
            previous: i64::try_from(previous_duration_ms).unwrap_or(i64::MAX),
            next: i64::try_from(next_duration_ms).unwrap_or(i64::MAX),
        })
    } else {
        Ok(())
    }
}
