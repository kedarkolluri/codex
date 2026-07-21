use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;

use super::*;

impl WorkflowRunModel {
    pub(super) fn reduce_group_begin(
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

    pub(super) fn reduce_group_end(
        &mut self,
        event: &WorkflowGroupEndEvent,
    ) -> Result<(), WorkflowModelError> {
        let node = self
            .topology
            .get(&event.group_id)
            .ok_or(WorkflowModelError::UnknownGroup {
                group_id: event.group_id,
            })?;
        let WorkflowTopologyNode::Group(group) = node;
        if group.state == WorkflowNodeState::Completed {
            return Err(WorkflowModelError::NodeAlreadyCompleted {
                node_id: event.group_id,
            });
        }
        if group.kind != event.kind || group.item_count != event.item_count {
            return Err(WorkflowModelError::GroupDefinitionMismatch {
                group_id: event.group_id,
            });
        }
        if let Some(node_id) = group.child_node_ids.iter().copied().find(|node_id| {
            self.topology
                .get(node_id)
                .is_some_and(|node| node.state() == WorkflowNodeState::Active)
        }) {
            return Err(WorkflowModelError::ActiveChildAtGroupEnd {
                group_id: event.group_id,
                node_id,
            });
        }

        let Some(WorkflowTopologyNode::Group(group)) = self.topology.get_mut(&event.group_id)
        else {
            return Err(WorkflowModelError::UnknownGroup {
                group_id: event.group_id,
            });
        };
        group.state = WorkflowNodeState::Completed;
        Ok(())
    }

    pub(super) fn ensure_phase_topology_inactive(
        &self,
        phase_index: u64,
    ) -> Result<(), WorkflowModelError> {
        if let Some(node_id) = self.topology.values().find_map(|node| {
            (node.phase_index() == phase_index && node.state() == WorkflowNodeState::Active)
                .then_some(node.id())
        }) {
            Err(WorkflowModelError::ActiveTopologyAtPhaseBoundary {
                phase_index,
                node_id,
            })
        } else {
            Ok(())
        }
    }

    fn insert_topology_node(
        &mut self,
        node: WorkflowTopologyNode,
    ) -> Result<(), WorkflowModelError> {
        let node_id = node.id();
        if self.topology.contains_key(&node_id) {
            return Err(WorkflowModelError::DuplicateTopologyId { node_id });
        }
        if node_id >= WORKFLOW_TOPOLOGY_MAX_NODES
            || self.next_topology_id >= WORKFLOW_TOPOLOGY_MAX_NODES
        {
            return Err(WorkflowModelError::TopologyLimitExceeded {
                maximum: WORKFLOW_TOPOLOGY_MAX_NODES,
            });
        }
        if node_id != self.next_topology_id {
            return Err(WorkflowModelError::UnexpectedTopologyId {
                expected: self.next_topology_id,
                actual: node_id,
            });
        }
        let next_topology_id = self.next_topology_id.checked_add(1).ok_or(
            WorkflowModelError::TopologyLimitExceeded {
                maximum: WORKFLOW_TOPOLOGY_MAX_NODES,
            },
        )?;
        let phase_index = node.phase_index();
        self.validate_parent(node_id, node.parent_node_id(), phase_index)?;
        let phase_position =
            usize::try_from(phase_index).map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;

        if let Some(parent_node_id) = node.parent_node_id() {
            let parent = self.topology.get_mut(&parent_node_id).ok_or(
                WorkflowModelError::MissingParent {
                    node_id,
                    parent_node_id,
                },
            )?;
            parent.child_node_ids_mut().push(node_id);
        } else {
            let phase = self
                .phases
                .get_mut(phase_position)
                .ok_or(WorkflowModelError::PhaseNotActive { phase_index })?;
            phase.root_node_ids.push(node_id);
        }
        self.topology.insert(node_id, node);
        self.next_topology_id = next_topology_id;
        Ok(())
    }

    fn validate_parent(
        &self,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase_index: u64,
    ) -> Result<(), WorkflowModelError> {
        let Some(parent_node_id) = parent_node_id else {
            return Ok(());
        };
        let parent =
            self.topology
                .get(&parent_node_id)
                .ok_or(WorkflowModelError::MissingParent {
                    node_id,
                    parent_node_id,
                })?;
        if parent.state() != WorkflowNodeState::Active {
            return Err(WorkflowModelError::ParentNotActive {
                node_id,
                parent_node_id,
            });
        }
        let parent_phase_index = parent.phase_index();
        if parent_phase_index != phase_index {
            return Err(WorkflowModelError::ParentPhaseMismatch {
                node_id,
                parent_node_id,
                phase_index,
                parent_phase_index,
            });
        }
        Ok(())
    }

    fn require_active_phase(&self) -> Result<u64, WorkflowModelError> {
        self.active_phase_index
            .ok_or(WorkflowModelError::NoActivePhase)
    }
}
