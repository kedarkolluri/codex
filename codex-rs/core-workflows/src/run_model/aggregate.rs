use codex_protocol::protocol::TokenUsage;

use super::WorkflowAggregate;
use super::WorkflowModelError;
use super::WorkflowNodeState;
use super::WorkflowTopologyNode;

impl WorkflowAggregate {
    pub(super) fn add_node(
        &mut self,
        node: &WorkflowTopologyNode,
    ) -> Result<(), WorkflowModelError> {
        match node {
            WorkflowTopologyNode::Group(_) => {
                self.group_count = checked_add_u64(self.group_count, 1, "group_count")?;
            }
            WorkflowTopologyNode::Agent(agent) => {
                self.agent_count = checked_add_u64(self.agent_count, 1, "agent_count")?;
                match agent.state {
                    WorkflowNodeState::Active => {
                        self.active_agent_count =
                            checked_add_u64(self.active_agent_count, 1, "active_agent_count")?;
                    }
                    WorkflowNodeState::Completed => {
                        self.completed_agent_count = checked_add_u64(
                            self.completed_agent_count,
                            1,
                            "completed_agent_count",
                        )?;
                    }
                }
                if agent.returned_null {
                    self.returned_null_count =
                        checked_add_u64(self.returned_null_count, 1, "returned_null_count")?;
                }
                add_token_usage(&mut self.token_usage, &agent.token_usage)?;
                self.tool_call_count = checked_add_u64(
                    self.tool_call_count,
                    agent.tool_call_count,
                    "tool_call_count",
                )?;
            }
        }
        Ok(())
    }
}

pub(super) fn validate_token_usage(
    node_id: u64,
    usage: &TokenUsage,
) -> Result<(), WorkflowModelError> {
    let fields = [
        ("input_tokens", usage.input_tokens),
        ("cached_input_tokens", usage.cached_input_tokens),
        ("output_tokens", usage.output_tokens),
        ("reasoning_output_tokens", usage.reasoning_output_tokens),
        ("total_tokens", usage.total_tokens),
    ];
    if let Some((field, value)) = fields.into_iter().find(|(_, value)| *value < 0) {
        Err(WorkflowModelError::NegativeTokenUsage {
            node_id,
            field,
            value,
        })
    } else {
        Ok(())
    }
}

fn add_token_usage(total: &mut TokenUsage, usage: &TokenUsage) -> Result<(), WorkflowModelError> {
    total.input_tokens = checked_add_i64(total.input_tokens, usage.input_tokens, "input_tokens")?;
    total.cached_input_tokens = checked_add_i64(
        total.cached_input_tokens,
        usage.cached_input_tokens,
        "cached_input_tokens",
    )?;
    total.output_tokens =
        checked_add_i64(total.output_tokens, usage.output_tokens, "output_tokens")?;
    total.reasoning_output_tokens = checked_add_i64(
        total.reasoning_output_tokens,
        usage.reasoning_output_tokens,
        "reasoning_output_tokens",
    )?;
    total.total_tokens = checked_add_i64(total.total_tokens, usage.total_tokens, "total_tokens")?;
    Ok(())
}

fn checked_add_u64(left: u64, right: u64, field: &'static str) -> Result<u64, WorkflowModelError> {
    left.checked_add(right)
        .ok_or(WorkflowModelError::AggregateOverflow { field })
}

fn checked_add_i64(left: i64, right: i64, field: &'static str) -> Result<i64, WorkflowModelError> {
    left.checked_add(right)
        .ok_or(WorkflowModelError::AggregateOverflow { field })
}
