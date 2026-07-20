//! Renderer-neutral projection of the stable workflow progress event stream.

use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentBoundEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use std::collections::BTreeMap;

mod aggregate;
mod event_validation;
mod types;

use aggregate::validate_token_usage;
use event_validation::validate_counter_progress;
use event_validation::validate_terminal_status;
pub use types::WorkflowAgent;
pub use types::WorkflowAggregate;
pub use types::WorkflowBudgetSummary;
pub use types::WorkflowGroup;
pub use types::WorkflowModelError;
pub use types::WorkflowNodeState;
pub use types::WorkflowPhase;
pub use types::WorkflowPhaseState;
pub use types::WorkflowRunState;
pub use types::WorkflowTopologyNode;

const IMPLICIT_ROOT_PHASE_TITLE: &str = "root";

/// Renderer-neutral state derived from one workflow run's ordered progress events.
///
/// Call [`WorkflowRunModel::from_event`] with the run's `RunBegin` event, then feed every later
/// event to [`WorkflowRunModel::apply`] in observation order. Invalid events are rejected
/// transactionally: an error leaves the model byte-for-byte equivalent to its prior state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRunModel {
    pub run_id: String,
    pub resumed_from_run_id: Option<String>,
    pub name: String,
    pub args_digest: String,
    pub state: WorkflowRunState,
    pub status: AgentStatus,
    pub terminal_reason: Option<WorkflowRunTerminalReason>,
    pub phases: Vec<WorkflowPhase>,
    pub topology: BTreeMap<u64, WorkflowTopologyNode>,
    /// All topology IDs in their source event order.
    pub topology_order: Vec<u64>,
    pub aggregate: WorkflowAggregate,
    pub budget: Option<WorkflowBudgetSummary>,
    active_phase_index: Option<u64>,
    next_phase_index: u64,
}

impl WorkflowRunModel {
    /// Starts a projection from a `RunBegin` event.
    pub fn from_event(event: &WorkflowEvent) -> Result<Self, WorkflowModelError> {
        let WorkflowEvent::RunBegin(event) = event else {
            return Err(WorkflowModelError::ExpectedRunBegin);
        };
        Self::from_run_begin(event)
    }

    /// Applies one subsequent event without changing the model if validation fails.
    pub fn apply(&mut self, event: &WorkflowEvent) -> Result<(), WorkflowModelError> {
        let mut candidate = self.clone();
        candidate.apply_inner(event)?;
        candidate.refresh_aggregates()?;
        *self = candidate;
        Ok(())
    }

    fn from_run_begin(event: &WorkflowRunBeginEvent) -> Result<Self, WorkflowModelError> {
        let phases = if event.phases.is_empty() {
            vec![WorkflowPhase {
                index: 0,
                title: IMPLICIT_ROOT_PHASE_TITLE.to_string(),
                state: WorkflowPhaseState::Active,
                implicit: true,
                root_node_ids: Vec::new(),
                aggregate: WorkflowAggregate::default(),
            }]
        } else {
            event
                .phases
                .iter()
                .enumerate()
                .map(|(index, title)| {
                    let index =
                        u64::try_from(index).map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
                    Ok(WorkflowPhase {
                        index,
                        title: title.clone(),
                        state: WorkflowPhaseState::Pending,
                        implicit: false,
                        root_node_ids: Vec::new(),
                        aggregate: WorkflowAggregate::default(),
                    })
                })
                .collect::<Result<Vec<_>, WorkflowModelError>>()?
        };
        let active_phase_index = phases.first().and_then(|phase| phase.implicit.then_some(0));

        Ok(Self {
            run_id: event.run_id.clone(),
            resumed_from_run_id: event.resumed_from_run_id.clone(),
            name: event.name.clone(),
            args_digest: event.args_digest.clone(),
            state: WorkflowRunState::Running,
            status: AgentStatus::Running,
            terminal_reason: None,
            phases,
            topology: BTreeMap::new(),
            topology_order: Vec::new(),
            aggregate: WorkflowAggregate::default(),
            budget: None,
            active_phase_index,
            next_phase_index: 0,
        })
    }

    fn apply_inner(&mut self, event: &WorkflowEvent) -> Result<(), WorkflowModelError> {
        if matches!(event, WorkflowEvent::RunBegin(_)) {
            return Err(WorkflowModelError::DuplicateRunBegin);
        }
        self.validate_run_id(event)?;
        if self.state == WorkflowRunState::Completed {
            return Err(WorkflowModelError::RunAlreadyCompleted);
        }

        match event {
            WorkflowEvent::RunBegin(_) => unreachable!("run begin rejected above"),
            WorkflowEvent::RunEnd(event) => self.apply_run_end(event),
            WorkflowEvent::PhaseBegin(event) => self.apply_phase_begin(event),
            WorkflowEvent::PhaseEnd(event) => self.apply_phase_end(event),
            WorkflowEvent::GroupBegin(event) => self.apply_group_begin(event),
            WorkflowEvent::GroupEnd(event) => self.apply_group_end(event),
            WorkflowEvent::AgentBegin(event) => self.apply_agent_begin(event),
            WorkflowEvent::AgentBound(event) => self.apply_agent_bound(event),
            WorkflowEvent::AgentUpdated(event) => self.apply_agent_updated(event),
            WorkflowEvent::AgentEnd(event) => self.apply_agent_end(event),
            WorkflowEvent::Log(_) => Ok(()),
        }
    }

    fn apply_run_end(&mut self, event: &WorkflowRunEndEvent) -> Result<(), WorkflowModelError> {
        if let Some(node_id) = self.topology.values().find_map(|node| match node {
            WorkflowTopologyNode::Group(group) if group.state == WorkflowNodeState::Active => {
                Some(group.id)
            }
            WorkflowTopologyNode::Agent(agent) if agent.state == WorkflowNodeState::Active => {
                Some(agent.id)
            }
            WorkflowTopologyNode::Group(_) | WorkflowTopologyNode::Agent(_) => None,
        }) {
            return Err(WorkflowModelError::ActiveTopologyAtRunEnd { node_id });
        }
        if event.spent < 0 || event.total.is_some_and(|total| total < 0) {
            return Err(WorkflowModelError::NegativeBudget {
                spent: event.spent,
                total: event.total,
            });
        }
        validate_terminal_status(/*node_id*/ None, &event.status)?;
        if let Some(active_phase_index) = self.active_phase_index.take() {
            self.phase_mut(active_phase_index)?.state = WorkflowPhaseState::Completed;
        }
        self.state = WorkflowRunState::Completed;
        self.status = event.status.clone();
        self.terminal_reason = event.terminal_reason;
        self.budget = Some(WorkflowBudgetSummary {
            spent: event.spent,
            total: event.total,
        });
        Ok(())
    }

    fn apply_phase_begin(
        &mut self,
        event: &WorkflowPhaseBeginEvent,
    ) -> Result<(), WorkflowModelError> {
        if let Some(active_phase_index) = self.active_phase_index {
            let active = self.phase(active_phase_index)?;
            if active.implicit && self.next_phase_index == 0 && event.phase_index == 0 {
                let phase = self.phase_mut(active_phase_index)?;
                phase.title.clone_from(&event.title);
                phase.implicit = false;
                self.next_phase_index = 1;
                return Ok(());
            }
            return Err(WorkflowModelError::PhaseAlreadyActive {
                phase_index: active_phase_index,
            });
        }
        if event.phase_index != self.next_phase_index {
            return Err(WorkflowModelError::UnexpectedPhaseIndex {
                expected: self.next_phase_index,
                actual: event.phase_index,
            });
        }

        let phase_count =
            u64::try_from(self.phases.len()).map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
        if event.phase_index == phase_count {
            self.phases.push(WorkflowPhase {
                index: event.phase_index,
                title: event.title.clone(),
                state: WorkflowPhaseState::Pending,
                implicit: false,
                root_node_ids: Vec::new(),
                aggregate: WorkflowAggregate::default(),
            });
        }
        let phase = self.phase_mut(event.phase_index)?;
        if phase.title != event.title {
            return Err(WorkflowModelError::PhaseTitleMismatch {
                phase_index: event.phase_index,
                expected: phase.title.clone(),
                actual: event.title.clone(),
            });
        }
        if phase.state != WorkflowPhaseState::Pending {
            return Err(WorkflowModelError::InvalidPhaseState {
                phase_index: event.phase_index,
                expected: WorkflowPhaseState::Pending,
                actual: phase.state,
            });
        }
        phase.state = WorkflowPhaseState::Active;
        self.active_phase_index = Some(event.phase_index);
        self.next_phase_index = self
            .next_phase_index
            .checked_add(1)
            .ok_or(WorkflowModelError::PhaseIndexOverflow)?;
        Ok(())
    }

    fn apply_phase_end(&mut self, event: &WorkflowPhaseEndEvent) -> Result<(), WorkflowModelError> {
        if self.active_phase_index != Some(event.phase_index) {
            return Err(WorkflowModelError::PhaseNotActive {
                phase_index: event.phase_index,
            });
        }
        if let Some(node_id) = self.topology.values().find_map(|node| {
            (node.phase_index() == event.phase_index
                && match node {
                    WorkflowTopologyNode::Group(group) => group.state == WorkflowNodeState::Active,
                    WorkflowTopologyNode::Agent(agent) => agent.state == WorkflowNodeState::Active,
                })
            .then_some(node.id())
        }) {
            return Err(WorkflowModelError::ActiveTopologyAtPhaseEnd {
                phase_index: event.phase_index,
                node_id,
            });
        }
        let phase = self.phase_mut(event.phase_index)?;
        if phase.title != event.title {
            return Err(WorkflowModelError::PhaseTitleMismatch {
                phase_index: event.phase_index,
                expected: phase.title.clone(),
                actual: event.title.clone(),
            });
        }
        phase.state = WorkflowPhaseState::Completed;
        self.active_phase_index = None;
        Ok(())
    }

    fn apply_group_begin(
        &mut self,
        event: &WorkflowGroupBeginEvent,
    ) -> Result<(), WorkflowModelError> {
        let phase_index = self.require_active_phase()?;
        self.insert_topology_node(WorkflowTopologyNode::Group(WorkflowGroup {
            id: event.group_id,
            parent_node_id: event.parent_node_id,
            phase_index,
            kind: event.kind,
            item_count: event.item_count,
            state: WorkflowNodeState::Active,
            child_node_ids: Vec::new(),
        }))
    }

    fn apply_group_end(&mut self, event: &WorkflowGroupEndEvent) -> Result<(), WorkflowModelError> {
        if let Some(node_id) = self.topology.values().find_map(|node| {
            (node.parent_node_id() == Some(event.group_id)
                && match node {
                    WorkflowTopologyNode::Group(group) => group.state == WorkflowNodeState::Active,
                    WorkflowTopologyNode::Agent(agent) => agent.state == WorkflowNodeState::Active,
                })
            .then_some(node.id())
        }) {
            return Err(WorkflowModelError::ActiveChildAtGroupEnd {
                group_id: event.group_id,
                node_id,
            });
        }
        let node =
            self.topology
                .get_mut(&event.group_id)
                .ok_or(WorkflowModelError::UnknownGroup {
                    group_id: event.group_id,
                })?;
        let WorkflowTopologyNode::Group(group) = node else {
            return Err(WorkflowModelError::TopologyKindMismatch {
                node_id: event.group_id,
                expected: "group",
            });
        };
        if group.kind != event.kind || group.item_count != event.item_count {
            return Err(WorkflowModelError::GroupDefinitionMismatch {
                group_id: event.group_id,
            });
        }
        if group.state == WorkflowNodeState::Completed {
            return Err(WorkflowModelError::NodeAlreadyCompleted {
                node_id: event.group_id,
            });
        }
        group.state = WorkflowNodeState::Completed;
        Ok(())
    }

    fn apply_agent_begin(
        &mut self,
        event: &WorkflowAgentBeginEvent,
    ) -> Result<(), WorkflowModelError> {
        let phase_index = self.require_active_phase()?;
        if let Some(event_phase) = &event.phase {
            let active_title = &self.phase(phase_index)?.title;
            if event_phase != active_title {
                return Err(WorkflowModelError::AgentPhaseMismatch {
                    node_id: event.node_id,
                    expected: active_title.clone(),
                    actual: event_phase.clone(),
                });
            }
        }
        if let Some(node) = self.topology.get_mut(&event.node_id) {
            let WorkflowTopologyNode::Agent(agent) = node else {
                return Err(WorkflowModelError::DuplicateTopologyId {
                    node_id: event.node_id,
                });
            };
            let expected = agent.attempt.saturating_add(1);
            if event.attempt != expected {
                return Err(WorkflowModelError::UnexpectedAgentAttempt {
                    node_id: event.node_id,
                    expected,
                    actual: event.attempt,
                });
            }
            if event.last_attempt_reason
                != Some(codex_protocol::protocol::WorkflowAgentAttemptReason::UserRetry)
            {
                return Err(WorkflowModelError::AgentAttemptReasonMismatch {
                    node_id: event.node_id,
                });
            }
            if agent.state == WorkflowNodeState::Completed {
                return Err(WorkflowModelError::NodeAlreadyCompleted {
                    node_id: event.node_id,
                });
            }
            if agent.parent_node_id != event.parent_node_id
                || agent.phase_index != phase_index
                || agent.label != event.label
                || agent.model != event.model
                || agent.effort != event.effort
            {
                return Err(WorkflowModelError::DuplicateTopologyId {
                    node_id: event.node_id,
                });
            }
            agent.attempt = event.attempt;
            agent.last_attempt_reason = event.last_attempt_reason;
            agent.child_thread_id = None;
            agent.status = AgentStatus::Running;
            agent.returned_null = false;
            return Ok(());
        }
        if event.attempt != 0 {
            return Err(WorkflowModelError::UnexpectedAgentAttempt {
                node_id: event.node_id,
                expected: 0,
                actual: event.attempt,
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
            status: AgentStatus::Running,
            token_usage: TokenUsage::default(),
            tool_call_count: 0,
            duration_ms: 0,
            returned_null: false,
            child_node_ids: Vec::new(),
        }))
    }

    fn apply_agent_bound(
        &mut self,
        event: &WorkflowAgentBoundEvent,
    ) -> Result<(), WorkflowModelError> {
        let agent = self.agent_mut(event.node_id)?;
        if event.attempt != agent.attempt {
            return Err(WorkflowModelError::UnexpectedAgentAttempt {
                node_id: event.node_id,
                expected: agent.attempt,
                actual: event.attempt,
            });
        }
        if let Some(child_thread_id) = &agent.child_thread_id {
            if child_thread_id == &event.child_thread_id {
                return Ok(());
            }
            return Err(WorkflowModelError::ConflictingAgentBinding {
                node_id: event.node_id,
                existing_child_thread_id: child_thread_id.clone(),
                child_thread_id: event.child_thread_id.clone(),
            });
        }
        if agent.state == WorkflowNodeState::Completed {
            return Err(WorkflowModelError::NodeAlreadyCompleted {
                node_id: event.node_id,
            });
        }
        agent.child_thread_id = Some(event.child_thread_id.clone());
        Ok(())
    }

    fn apply_agent_updated(
        &mut self,
        event: &WorkflowAgentUpdatedEvent,
    ) -> Result<(), WorkflowModelError> {
        validate_token_usage(event.node_id, &event.token_usage)?;
        let agent = self.agent_mut(event.node_id)?;
        if event.attempt != agent.attempt {
            return Err(WorkflowModelError::UnexpectedAgentAttempt {
                node_id: event.node_id,
                expected: agent.attempt,
                actual: event.attempt,
            });
        }
        if event.last_attempt_reason != agent.last_attempt_reason {
            return Err(WorkflowModelError::AgentAttemptReasonMismatch {
                node_id: event.node_id,
            });
        }
        if agent.child_thread_id.is_none() {
            return Err(WorkflowModelError::AgentNotBound {
                node_id: event.node_id,
            });
        }
        if agent.state == WorkflowNodeState::Completed {
            return Err(WorkflowModelError::NodeAlreadyCompleted {
                node_id: event.node_id,
            });
        }
        validate_counter_progress(
            event.node_id,
            &agent.token_usage,
            agent.tool_call_count,
            agent.duration_ms,
            &event.token_usage,
            event.tool_call_count,
            event.duration_ms,
        )?;
        agent.token_usage.clone_from(&event.token_usage);
        agent.tool_call_count = event.tool_call_count;
        agent.duration_ms = event.duration_ms;
        Ok(())
    }

    fn apply_agent_end(&mut self, event: &WorkflowAgentEndEvent) -> Result<(), WorkflowModelError> {
        validate_token_usage(event.node_id, &event.token_usage)?;
        validate_terminal_status(Some(event.node_id), &event.status)?;
        let agent = self.agent_mut(event.node_id)?;
        if event.attempt != agent.attempt {
            return Err(WorkflowModelError::UnexpectedAgentAttempt {
                node_id: event.node_id,
                expected: agent.attempt,
                actual: event.attempt,
            });
        }
        if agent.child_thread_id.is_none() {
            return Err(WorkflowModelError::AgentNotBound {
                node_id: event.node_id,
            });
        }
        if agent.state == WorkflowNodeState::Completed {
            return Err(WorkflowModelError::NodeAlreadyCompleted {
                node_id: event.node_id,
            });
        }
        validate_counter_progress(
            event.node_id,
            &agent.token_usage,
            agent.tool_call_count,
            agent.duration_ms,
            &event.token_usage,
            event.tool_call_count,
            event.duration_ms,
        )?;
        agent.state = WorkflowNodeState::Completed;
        agent.status.clone_from(&event.status);
        agent.last_attempt_reason = event.last_attempt_reason;
        agent.token_usage.clone_from(&event.token_usage);
        agent.tool_call_count = event.tool_call_count;
        agent.duration_ms = event.duration_ms;
        agent.returned_null = event.returned_null;
        Ok(())
    }

    fn insert_topology_node(
        &mut self,
        node: WorkflowTopologyNode,
    ) -> Result<(), WorkflowModelError> {
        let node_id = node.id();
        if self.topology.contains_key(&node_id) {
            return Err(WorkflowModelError::DuplicateTopologyId { node_id });
        }
        let phase_index = node.phase_index();
        let parent_node_id = node.parent_node_id();
        let is_phase_root = match parent_node_id {
            Some(parent_node_id) => {
                let parent = self.topology.get_mut(&parent_node_id).ok_or(
                    WorkflowModelError::MissingParent {
                        node_id,
                        parent_node_id,
                    },
                )?;
                let parent_phase_index = parent.phase_index();
                if parent_phase_index != phase_index {
                    return Err(WorkflowModelError::ParentPhaseMismatch {
                        node_id,
                        parent_node_id,
                        phase_index,
                        parent_phase_index,
                    });
                }
                parent.child_node_ids_mut().push(node_id);
                false
            }
            None => true,
        };
        if is_phase_root {
            self.phase_mut(phase_index)?.root_node_ids.push(node_id);
        }
        self.topology.insert(node_id, node);
        self.topology_order.push(node_id);
        Ok(())
    }

    fn refresh_aggregates(&mut self) -> Result<(), WorkflowModelError> {
        let mut phase_aggregates = vec![WorkflowAggregate::default(); self.phases.len()];
        let mut run_aggregate = WorkflowAggregate::default();
        for node in self.topology.values() {
            let phase_index = usize::try_from(node.phase_index())
                .map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
            let phase_aggregate =
                phase_aggregates
                    .get_mut(phase_index)
                    .ok_or(WorkflowModelError::UnknownPhase {
                        phase_index: node.phase_index(),
                    })?;
            phase_aggregate.add_node(node)?;
            run_aggregate.add_node(node)?;
        }
        for (phase, aggregate) in self.phases.iter_mut().zip(phase_aggregates) {
            phase.aggregate = aggregate;
        }
        self.aggregate = run_aggregate;
        Ok(())
    }
}

#[cfg(test)]
#[path = "run_model_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "run_model_validation_tests.rs"]
mod validation_tests;
