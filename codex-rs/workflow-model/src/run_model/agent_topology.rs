use codex_code_mode_protocol::WORKFLOW_AGENT_LABEL_MAX_BYTES;
use codex_code_mode_protocol::WORKFLOW_AGENT_MAX_RETRIES;
use codex_code_mode_protocol::WORKFLOW_AGENT_OPTION_MAX_BYTES;
use codex_code_mode_protocol::WORKFLOW_PHASE_TITLE_MAX_BYTES;
use codex_protocol::ThreadId;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentBoundEvent;

use super::*;

impl WorkflowRunModel {
    pub(super) fn reduce_agent_begin(
        &mut self,
        event: &WorkflowAgentBeginEvent,
    ) -> Result<(), WorkflowModelError> {
        let phase_index = self.require_active_phase()?;
        let phase_position =
            usize::try_from(phase_index).map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
        let phase = self
            .phases
            .get(phase_position)
            .ok_or(WorkflowModelError::PhaseNotActive { phase_index })?;
        if let Some(event_phase) = &event.phase {
            validate_text("agent phase", event_phase, WORKFLOW_PHASE_TITLE_MAX_BYTES)?;
            if event_phase != &phase.title {
                return Err(WorkflowModelError::AgentPhaseMismatch {
                    node_id: event.node_id,
                    expected: phase.title.clone(),
                    actual: event_phase.clone(),
                });
            }
        }
        validate_text("agent label", &event.label, WORKFLOW_AGENT_LABEL_MAX_BYTES)?;
        validate_text("agent model", &event.model, WORKFLOW_AGENT_OPTION_MAX_BYTES)?;
        validate_text(
            "agent effort",
            event.effort.as_str(),
            WORKFLOW_AGENT_OPTION_MAX_BYTES,
        )?;

        if let Some(node) = self.topology.get(&event.node_id) {
            let WorkflowTopologyNode::Agent(agent) = node else {
                return Err(WorkflowModelError::DuplicateTopologyId {
                    node_id: event.node_id,
                });
            };
            validate_agent_retry(event, phase_index, agent)?;
            self.validate_parent(event.node_id, event.parent_node_id, phase_index)?;
            let Some(WorkflowTopologyNode::Agent(agent)) = self.topology.get_mut(&event.node_id)
            else {
                return Err(WorkflowModelError::UnknownAgent {
                    node_id: event.node_id,
                });
            };
            agent.attempt = event.attempt;
            agent.last_attempt_reason = event.last_attempt_reason;
            agent.child_thread_id = None;
            return Ok(());
        }

        if event.attempt != 0 {
            return Err(WorkflowModelError::UnexpectedAgentAttempt {
                node_id: event.node_id,
                expected: 0,
                actual: event.attempt,
            });
        }
        if event.last_attempt_reason.is_some() {
            return Err(WorkflowModelError::AgentAttemptReasonMismatch {
                node_id: event.node_id,
            });
        }
        self.insert_topology_node(WorkflowTopologyNode::Agent(WorkflowAgent {
            id: event.node_id,
            attempt: event.attempt,
            last_attempt_reason: event.last_attempt_reason,
            parent_node_id: event.parent_node_id,
            phase_index,
            label: event.label.clone(),
            model: event.model.clone(),
            effort: event.effort.clone(),
            child_thread_id: None,
            state: WorkflowNodeState::Active,
            child_node_ids: Vec::new(),
        }))
    }

    pub(super) fn reduce_agent_bound(
        &mut self,
        event: &WorkflowAgentBoundEvent,
    ) -> Result<(), WorkflowModelError> {
        let node = self
            .topology
            .get(&event.node_id)
            .ok_or(WorkflowModelError::UnknownAgent {
                node_id: event.node_id,
            })?;
        let WorkflowTopologyNode::Agent(agent) = node else {
            return Err(WorkflowModelError::TopologyKindMismatch {
                node_id: event.node_id,
                expected: "agent",
            });
        };
        if agent.state == WorkflowNodeState::Completed {
            return Err(WorkflowModelError::NodeAlreadyCompleted {
                node_id: event.node_id,
            });
        }
        if event.attempt != agent.attempt {
            return Err(WorkflowModelError::UnexpectedAgentAttempt {
                node_id: event.node_id,
                expected: agent.attempt,
                actual: event.attempt,
            });
        }
        let child_thread_id = ThreadId::from_string(&event.child_thread_id).map_err(|_| {
            WorkflowModelError::InvalidChildThreadId {
                node_id: event.node_id,
            }
        })?;
        if let Some(existing_child_thread_id) = agent.child_thread_id {
            if existing_child_thread_id == child_thread_id {
                return Ok(());
            }
            return Err(WorkflowModelError::ConflictingAgentBinding {
                node_id: event.node_id,
                existing_child_thread_id,
                child_thread_id,
            });
        }
        if let Some(existing_node_id) = self.topology.iter().find_map(|(node_id, node)| {
            let WorkflowTopologyNode::Agent(agent) = node else {
                return None;
            };
            (agent.child_thread_id == Some(child_thread_id)).then_some(*node_id)
        }) {
            return Err(WorkflowModelError::ChildThreadAlreadyBound {
                node_id: event.node_id,
                existing_node_id,
                child_thread_id,
            });
        }

        let Some(WorkflowTopologyNode::Agent(agent)) = self.topology.get_mut(&event.node_id) else {
            return Err(WorkflowModelError::UnknownAgent {
                node_id: event.node_id,
            });
        };
        agent.child_thread_id = Some(child_thread_id);
        Ok(())
    }
}

fn validate_agent_retry(
    event: &WorkflowAgentBeginEvent,
    phase_index: u64,
    agent: &WorkflowAgent,
) -> Result<(), WorkflowModelError> {
    if agent.state == WorkflowNodeState::Completed {
        return Err(WorkflowModelError::NodeAlreadyCompleted {
            node_id: event.node_id,
        });
    }
    if event.attempt > WORKFLOW_AGENT_MAX_RETRIES {
        return Err(WorkflowModelError::AgentRetryLimitExceeded {
            node_id: event.node_id,
            maximum: WORKFLOW_AGENT_MAX_RETRIES,
        });
    }
    let expected =
        agent
            .attempt
            .checked_add(1)
            .ok_or(WorkflowModelError::AgentRetryLimitExceeded {
                node_id: event.node_id,
                maximum: WORKFLOW_AGENT_MAX_RETRIES,
            })?;
    if event.attempt != expected {
        return Err(WorkflowModelError::UnexpectedAgentAttempt {
            node_id: event.node_id,
            expected,
            actual: event.attempt,
        });
    }
    if event.last_attempt_reason != Some(WorkflowAgentAttemptReason::UserRetry) {
        return Err(WorkflowModelError::AgentAttemptReasonMismatch {
            node_id: event.node_id,
        });
    }
    if agent.parent_node_id != event.parent_node_id
        || agent.phase_index != phase_index
        || agent.label != event.label
        || agent.model != event.model
        || agent.effort != event.effort
    {
        return Err(WorkflowModelError::AgentDefinitionMismatch {
            node_id: event.node_id,
        });
    }
    Ok(())
}
