use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;

use super::*;

pub(super) struct PendingAggregateUpdate {
    phase_position: usize,
    run: WorkflowAggregate,
    phase: WorkflowAggregate,
}

impl WorkflowRunModel {
    pub(super) fn prepare_aggregate_update(
        &self,
        phase_index: u64,
        update: impl Fn(&mut WorkflowAggregate) -> Result<(), WorkflowModelError>,
    ) -> Result<PendingAggregateUpdate, WorkflowModelError> {
        let phase_position =
            usize::try_from(phase_index).map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
        let phase = self
            .phases
            .get(phase_position)
            .ok_or(WorkflowModelError::PhaseNotActive { phase_index })?;
        let mut run = self.aggregate.clone();
        update(&mut run)?;
        let mut phase = phase.aggregate.clone();
        update(&mut phase)?;
        Ok(PendingAggregateUpdate {
            phase_position,
            run,
            phase,
        })
    }

    pub(super) fn commit_aggregate_update(&mut self, update: PendingAggregateUpdate) {
        self.aggregate = update.run;
        self.phases[update.phase_position].aggregate = update.phase;
    }
}

impl WorkflowAggregate {
    pub(super) fn add_node(
        &mut self,
        node: &WorkflowTopologyNode,
    ) -> Result<(), WorkflowModelError> {
        match node {
            WorkflowTopologyNode::Group(_) => {
                self.group_count =
                    checked_add_u64(self.group_count, /*right*/ 1, "group_count")?;
            }
            WorkflowTopologyNode::Agent(agent) => {
                self.agent_count =
                    checked_add_u64(self.agent_count, /*right*/ 1, "agent_count")?;
                match agent.state {
                    WorkflowNodeState::Active => {
                        self.active_agent_count = checked_add_u64(
                            self.active_agent_count,
                            /*right*/ 1,
                            "active_agent_count",
                        )?;
                    }
                    WorkflowNodeState::Completed => {
                        self.completed_agent_count = checked_add_u64(
                            self.completed_agent_count,
                            /*right*/ 1,
                            "completed_agent_count",
                        )?;
                    }
                }
                if agent.returned_null {
                    self.returned_null_count = checked_add_u64(
                        self.returned_null_count,
                        /*right*/ 1,
                        "returned_null_count",
                    )?;
                }
                self.add_agent_counters(
                    &TokenUsage::default(),
                    /*previous_tool_call_count*/ 0,
                    /*previous_duration_ms*/ 0,
                    &agent.token_usage,
                    agent.tool_call_count,
                    agent.duration_ms,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn apply_agent_updated(
        &mut self,
        agent: &WorkflowAgent,
        event: &WorkflowAgentUpdatedEvent,
    ) -> Result<(), WorkflowModelError> {
        self.add_agent_counters(
            &agent.token_usage,
            agent.tool_call_count,
            agent.duration_ms,
            &event.token_usage,
            event.tool_call_count,
            event.duration_ms,
        )
    }

    pub(super) fn apply_agent_end(
        &mut self,
        agent: &WorkflowAgent,
        event: &WorkflowAgentEndEvent,
    ) -> Result<(), WorkflowModelError> {
        self.add_agent_counters(
            &agent.token_usage,
            agent.tool_call_count,
            agent.duration_ms,
            &event.token_usage,
            event.tool_call_count,
            event.duration_ms,
        )?;
        self.active_agent_count = checked_sub_u64(
            self.active_agent_count,
            /*right*/ 1,
            "active_agent_count",
        )?;
        self.completed_agent_count = checked_add_u64(
            self.completed_agent_count,
            /*right*/ 1,
            "completed_agent_count",
        )?;
        if event.returned_null {
            self.returned_null_count = checked_add_u64(
                self.returned_null_count,
                /*right*/ 1,
                "returned_null_count",
            )?;
        }
        Ok(())
    }

    fn add_agent_counters(
        &mut self,
        previous_usage: &TokenUsage,
        previous_tool_call_count: u64,
        previous_duration_ms: u64,
        actual_usage: &TokenUsage,
        actual_tool_call_count: u64,
        actual_duration_ms: u64,
    ) -> Result<(), WorkflowModelError> {
        add_token_usage_delta(&mut self.token_usage, previous_usage, actual_usage)?;
        self.tool_call_count = checked_add_u64_delta(
            self.tool_call_count,
            previous_tool_call_count,
            actual_tool_call_count,
            "tool_call_count",
        )?;
        self.duration_ms = checked_add_u64_delta(
            self.duration_ms,
            previous_duration_ms,
            actual_duration_ms,
            "duration_ms",
        )?;
        Ok(())
    }
}

fn add_token_usage_delta(
    total: &mut TokenUsage,
    previous: &TokenUsage,
    actual: &TokenUsage,
) -> Result<(), WorkflowModelError> {
    total.input_tokens = checked_add_i64_delta(
        total.input_tokens,
        previous.input_tokens,
        actual.input_tokens,
        "input_tokens",
    )?;
    total.cached_input_tokens = checked_add_i64_delta(
        total.cached_input_tokens,
        previous.cached_input_tokens,
        actual.cached_input_tokens,
        "cached_input_tokens",
    )?;
    total.cache_write_input_tokens = checked_add_i64_delta(
        total.cache_write_input_tokens,
        previous.cache_write_input_tokens,
        actual.cache_write_input_tokens,
        "cache_write_input_tokens",
    )?;
    total.output_tokens = checked_add_i64_delta(
        total.output_tokens,
        previous.output_tokens,
        actual.output_tokens,
        "output_tokens",
    )?;
    total.reasoning_output_tokens = checked_add_i64_delta(
        total.reasoning_output_tokens,
        previous.reasoning_output_tokens,
        actual.reasoning_output_tokens,
        "reasoning_output_tokens",
    )?;
    total.total_tokens = checked_add_i64_delta(
        total.total_tokens,
        previous.total_tokens,
        actual.total_tokens,
        "total_tokens",
    )?;
    Ok(())
}

fn checked_add_i64_delta(
    total: i64,
    previous: i64,
    actual: i64,
    field: &'static str,
) -> Result<i64, WorkflowModelError> {
    let delta = actual
        .checked_sub(previous)
        .ok_or(WorkflowModelError::AggregateOverflow { field })?;
    if delta < 0 {
        return Err(WorkflowModelError::AggregateUnderflow { field });
    }
    total
        .checked_add(delta)
        .ok_or(WorkflowModelError::AggregateOverflow { field })
}

fn checked_add_u64_delta(
    total: u64,
    previous: u64,
    actual: u64,
    field: &'static str,
) -> Result<u64, WorkflowModelError> {
    let delta = actual
        .checked_sub(previous)
        .ok_or(WorkflowModelError::AggregateUnderflow { field })?;
    checked_add_u64(total, delta, field)
}

fn checked_add_u64(left: u64, right: u64, field: &'static str) -> Result<u64, WorkflowModelError> {
    left.checked_add(right)
        .ok_or(WorkflowModelError::AggregateOverflow { field })
}

fn checked_sub_u64(left: u64, right: u64, field: &'static str) -> Result<u64, WorkflowModelError> {
    left.checked_sub(right)
        .ok_or(WorkflowModelError::AggregateUnderflow { field })
}
