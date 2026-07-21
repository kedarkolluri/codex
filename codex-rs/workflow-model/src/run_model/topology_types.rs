use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowGroupKind;

/// Lifecycle shared by workflow group and agent topology nodes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowNodeState {
    Active,
    Completed,
}

/// A parallel or pipeline group in the per-run topology namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowGroup {
    pub(super) id: u64,
    pub(super) parent_node_id: Option<u64>,
    pub(super) phase_index: u64,
    pub(super) kind: WorkflowGroupKind,
    pub(super) item_count: u64,
    pub(super) state: WorkflowNodeState,
    pub(super) child_node_ids: Vec<u64>,
}

impl WorkflowGroup {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn parent_node_id(&self) -> Option<u64> {
        self.parent_node_id
    }

    pub fn phase_index(&self) -> u64 {
        self.phase_index
    }

    pub fn kind(&self) -> WorkflowGroupKind {
        self.kind
    }

    pub fn item_count(&self) -> u64 {
        self.item_count
    }

    pub fn state(&self) -> WorkflowNodeState {
        self.state
    }

    pub fn child_node_ids(&self) -> &[u64] {
        &self.child_node_ids
    }
}

/// One logical workflow agent whose identity is preserved across bounded retry generations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowAgent {
    pub(super) id: u64,
    pub(super) attempt: u32,
    pub(super) last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub(super) parent_node_id: Option<u64>,
    pub(super) phase_index: u64,
    pub(super) label: String,
    pub(super) model: String,
    pub(super) effort: ReasoningEffort,
    pub(super) child_thread_id: Option<ThreadId>,
    pub(super) state: WorkflowNodeState,
    pub(super) child_node_ids: Vec<u64>,
}

impl WorkflowAgent {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn last_attempt_reason(&self) -> Option<WorkflowAgentAttemptReason> {
        self.last_attempt_reason
    }

    pub fn parent_node_id(&self) -> Option<u64> {
        self.parent_node_id
    }

    pub fn phase_index(&self) -> u64 {
        self.phase_index
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn effort(&self) -> &ReasoningEffort {
        &self.effort
    }

    pub fn child_thread_id(&self) -> Option<ThreadId> {
        self.child_thread_id
    }

    pub fn state(&self) -> WorkflowNodeState {
        self.state
    }

    pub fn child_node_ids(&self) -> &[u64] {
        &self.child_node_ids
    }
}

/// A node from the deterministic namespace shared by groups and agents.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WorkflowTopologyNode {
    Group(WorkflowGroup),
    Agent(WorkflowAgent),
}

impl WorkflowTopologyNode {
    pub fn id(&self) -> u64 {
        match self {
            Self::Group(group) => group.id,
            Self::Agent(agent) => agent.id,
        }
    }

    pub fn parent_node_id(&self) -> Option<u64> {
        match self {
            Self::Group(group) => group.parent_node_id,
            Self::Agent(agent) => agent.parent_node_id,
        }
    }

    pub fn phase_index(&self) -> u64 {
        match self {
            Self::Group(group) => group.phase_index,
            Self::Agent(agent) => agent.phase_index,
        }
    }

    pub fn state(&self) -> WorkflowNodeState {
        match self {
            Self::Group(group) => group.state,
            Self::Agent(agent) => agent.state,
        }
    }

    pub fn child_node_ids(&self) -> &[u64] {
        match self {
            Self::Group(group) => &group.child_node_ids,
            Self::Agent(agent) => &agent.child_node_ids,
        }
    }

    pub(super) fn child_node_ids_mut(&mut self) -> &mut Vec<u64> {
        match self {
            Self::Group(group) => &mut group.child_node_ids,
            Self::Agent(agent) => &mut agent.child_node_ids,
        }
    }
}
