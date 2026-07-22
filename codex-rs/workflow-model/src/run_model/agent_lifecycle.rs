use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;

use super::*;

impl WorkflowRunModel {
    pub(super) fn reduce_agent_updated(
        &mut self,
        event: &WorkflowAgentUpdatedEvent,
    ) -> Result<(), WorkflowModelError> {
        let agent = self.require_active_bound_agent(event.node_id, event.attempt)?;
        if event.last_attempt_reason != agent.last_attempt_reason {
            return Err(WorkflowModelError::AgentAttemptReasonMismatch {
                node_id: event.node_id,
            });
        }
        validate_token_usage(event.node_id, &event.token_usage)?;
        validate_counter_progress(
            event.node_id,
            &agent.token_usage,
            agent.tool_call_count,
            agent.duration_ms,
            &event.token_usage,
            event.tool_call_count,
            event.duration_ms,
        )?;
        let aggregate_update = self.prepare_aggregate_update(agent.phase_index, |aggregate| {
            aggregate.apply_agent_updated(agent, event)
        })?;

        let agent = self.agent_mut(event.node_id)?;
        agent.token_usage.clone_from(&event.token_usage);
        agent.tool_call_count = event.tool_call_count;
        agent.duration_ms = event.duration_ms;
        self.commit_aggregate_update(aggregate_update);
        Ok(())
    }

    pub(super) fn reduce_agent_end(
        &mut self,
        event: &WorkflowAgentEndEvent,
    ) -> Result<(), WorkflowModelError> {
        let agent = self.require_active_bound_agent(event.node_id, event.attempt)?;
        if let Some(child_node_id) = agent.child_node_ids.iter().copied().find(|child_node_id| {
            self.topology
                .get(child_node_id)
                .is_some_and(|child| child.state() == WorkflowNodeState::Active)
        }) {
            return Err(WorkflowModelError::ActiveChildAtAgentEnd {
                node_id: event.node_id,
                child_node_id,
            });
        }
        validate_terminal_status(Some(event.node_id), &event.status)?;
        validate_token_usage(event.node_id, &event.token_usage)?;
        validate_counter_progress(
            event.node_id,
            &agent.token_usage,
            agent.tool_call_count,
            agent.duration_ms,
            &event.token_usage,
            event.tool_call_count,
            event.duration_ms,
        )?;
        let aggregate_update = self.prepare_aggregate_update(agent.phase_index, |aggregate| {
            aggregate.apply_agent_end(agent, event)
        })?;

        let agent = self.agent_mut(event.node_id)?;
        agent.state = WorkflowNodeState::Completed;
        agent.status.clone_from(&event.status);
        agent.last_attempt_reason = event.last_attempt_reason;
        agent.token_usage.clone_from(&event.token_usage);
        agent.tool_call_count = event.tool_call_count;
        agent.duration_ms = event.duration_ms;
        agent.returned_null = event.returned_null;
        self.commit_aggregate_update(aggregate_update);
        Ok(())
    }

    fn require_active_bound_agent(
        &self,
        node_id: u64,
        attempt: u32,
    ) -> Result<&WorkflowAgent, WorkflowModelError> {
        let node = self
            .topology
            .get(&node_id)
            .ok_or(WorkflowModelError::UnknownAgent { node_id })?;
        let WorkflowTopologyNode::Agent(agent) = node else {
            return Err(WorkflowModelError::TopologyKindMismatch {
                node_id,
                expected: "agent",
            });
        };
        if agent.state == WorkflowNodeState::Completed {
            return Err(WorkflowModelError::NodeAlreadyCompleted { node_id });
        }
        if attempt != agent.attempt {
            return Err(WorkflowModelError::UnexpectedAgentAttempt {
                node_id,
                expected: agent.attempt,
                actual: attempt,
            });
        }
        if agent.child_thread_id.is_none() {
            return Err(WorkflowModelError::AgentNotBound { node_id });
        }
        Ok(agent)
    }

    fn agent_mut(&mut self, node_id: u64) -> Result<&mut WorkflowAgent, WorkflowModelError> {
        let node = self
            .topology
            .get_mut(&node_id)
            .ok_or(WorkflowModelError::UnknownAgent { node_id })?;
        let WorkflowTopologyNode::Agent(agent) = node else {
            return Err(WorkflowModelError::TopologyKindMismatch {
                node_id,
                expected: "agent",
            });
        };
        Ok(agent)
    }
}

fn validate_terminal_status(
    node_id: Option<u64>,
    status: &AgentStatus,
) -> Result<(), WorkflowModelError> {
    match status {
        AgentStatus::PendingInit | AgentStatus::Running => {
            Err(WorkflowModelError::NonTerminalStatus {
                node_id,
                status: status.clone(),
            })
        }
        AgentStatus::Interrupted
        | AgentStatus::Completed(_)
        | AgentStatus::Errored(_)
        | AgentStatus::Shutdown
        | AgentStatus::NotFound => Ok(()),
    }
}

fn validate_token_usage(node_id: u64, usage: &TokenUsage) -> Result<(), WorkflowModelError> {
    if let Some((field, value)) = token_counters(usage)
        .into_iter()
        .find(|(_, value)| *value < 0)
    {
        Err(WorkflowModelError::NegativeTokenUsage {
            node_id,
            field,
            value,
        })
    } else {
        Ok(())
    }
}

fn validate_counter_progress(
    node_id: u64,
    previous_usage: &TokenUsage,
    previous_tool_call_count: u64,
    previous_duration_ms: u64,
    actual_usage: &TokenUsage,
    actual_tool_call_count: u64,
    actual_duration_ms: u64,
) -> Result<(), WorkflowModelError> {
    if let Some(((field, previous), (_, actual))) = token_counters(previous_usage)
        .into_iter()
        .zip(token_counters(actual_usage))
        .find(|((_, previous), (_, actual))| actual < previous)
    {
        return Err(WorkflowModelError::TokenCounterRegression {
            node_id,
            field,
            previous,
            actual,
        });
    }
    for (field, previous, actual) in [
        (
            "tool_call_count",
            previous_tool_call_count,
            actual_tool_call_count,
        ),
        ("duration_ms", previous_duration_ms, actual_duration_ms),
    ] {
        if actual < previous {
            return Err(WorkflowModelError::UnsignedCounterRegression {
                node_id,
                field,
                previous,
                actual,
            });
        }
    }
    Ok(())
}

fn token_counters(usage: &TokenUsage) -> [(&'static str, i64); 6] {
    [
        ("input_tokens", usage.input_tokens),
        ("cached_input_tokens", usage.cached_input_tokens),
        ("cache_write_input_tokens", usage.cache_write_input_tokens),
        ("output_tokens", usage.output_tokens),
        ("reasoning_output_tokens", usage.reasoning_output_tokens),
        ("total_tokens", usage.total_tokens),
    ]
}
