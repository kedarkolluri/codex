use std::fmt;

use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowGroupKind;

/// Lifecycle of the workflow run represented by the workflow projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowRunState {
    Running,
    Completed,
}

/// Lifecycle of one declared, dynamic, or implicit workflow phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowPhaseState {
    Pending,
    Active,
    Completed,
}

/// Lifecycle shared by workflow group and agent topology nodes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowNodeState {
    Active,
    Completed,
}

/// Rolled-up counters for a workflow run or phase.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkflowAggregate {
    pub group_count: u64,
    pub agent_count: u64,
    pub active_agent_count: u64,
    pub completed_agent_count: u64,
    pub returned_null_count: u64,
    pub token_usage: TokenUsage,
    pub tool_call_count: u64,
}

/// Terminal weighted-token budget values reported by the workflow runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkflowBudgetSummary {
    pub spent: i64,
    pub total: Option<i64>,
}

/// One phase in the declared and dynamically extended phase order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowPhase {
    pub index: u64,
    pub title: String,
    pub state: WorkflowPhaseState,
    /// True only for the synthetic phase used when a script declares and calls no phases.
    pub implicit: bool,
    /// Topology roots as viewed within this phase, in source event order.
    pub root_node_ids: Vec<u64>,
    pub aggregate: WorkflowAggregate,
}

/// A parallel or pipeline group in the per-run topology namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowGroup {
    pub id: u64,
    pub parent_node_id: Option<u64>,
    pub phase_index: u64,
    pub kind: WorkflowGroupKind,
    pub item_count: u64,
    pub state: WorkflowNodeState,
    pub child_node_ids: Vec<u64>,
}

/// An agent in the per-run topology namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowAgent {
    pub id: u64,
    /// Exact zero-based live generation for selected-attempt controls.
    pub attempt: u32,
    /// Most recent retry/skip reason carried by the progress stream.
    pub last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub parent_node_id: Option<u64>,
    pub phase_index: u64,
    pub label: String,
    pub model: String,
    pub effort: ReasoningEffort,
    /// Persistent child thread bound after the child is registered.
    pub child_thread_id: Option<String>,
    pub state: WorkflowNodeState,
    pub status: AgentStatus,
    pub token_usage: TokenUsage,
    pub tool_call_count: u64,
    /// Aggregate wall-clock duration across the initial attempt and every retry.
    pub duration_ms: u64,
    pub returned_null: bool,
    pub child_node_ids: Vec<u64>,
}

/// A node from the deterministic namespace shared by groups and agents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowTopologyNode {
    Group(WorkflowGroup),
    Agent(WorkflowAgent),
}

impl WorkflowTopologyNode {
    /// Returns this node's shared topology ID.
    pub fn id(&self) -> u64 {
        match self {
            Self::Group(group) => group.id,
            Self::Agent(agent) => agent.id,
        }
    }

    /// Returns the phase that was active when this node was created.
    pub fn phase_index(&self) -> u64 {
        match self {
            Self::Group(group) => group.phase_index,
            Self::Agent(agent) => agent.phase_index,
        }
    }

    /// Returns the parent from the shared topology namespace, if any.
    pub fn parent_node_id(&self) -> Option<u64> {
        match self {
            Self::Group(group) => group.parent_node_id,
            Self::Agent(agent) => agent.parent_node_id,
        }
    }

    /// Returns child IDs in source event order.
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

/// A malformed or out-of-order event that cannot safely update a run projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowModelError {
    ExpectedRunBegin,
    DuplicateRunBegin,
    RunIdMismatch {
        expected: String,
        actual: String,
    },
    RunAlreadyCompleted,
    PhaseIndexOverflow,
    UnexpectedPhaseIndex {
        expected: u64,
        actual: u64,
    },
    UnknownPhase {
        phase_index: u64,
    },
    PhaseTitleMismatch {
        phase_index: u64,
        expected: String,
        actual: String,
    },
    PhaseAlreadyActive {
        phase_index: u64,
    },
    PhaseNotActive {
        phase_index: u64,
    },
    ActiveTopologyAtPhaseEnd {
        phase_index: u64,
        node_id: u64,
    },
    InvalidPhaseState {
        phase_index: u64,
        expected: WorkflowPhaseState,
        actual: WorkflowPhaseState,
    },
    NoActivePhase,
    AgentPhaseMismatch {
        node_id: u64,
        expected: String,
        actual: String,
    },
    DuplicateTopologyId {
        node_id: u64,
    },
    MissingParent {
        node_id: u64,
        parent_node_id: u64,
    },
    ParentPhaseMismatch {
        node_id: u64,
        parent_node_id: u64,
        phase_index: u64,
        parent_phase_index: u64,
    },
    UnknownGroup {
        group_id: u64,
    },
    UnknownAgent {
        node_id: u64,
    },
    AgentNotBound {
        node_id: u64,
    },
    UnexpectedAgentAttempt {
        node_id: u64,
        expected: u32,
        actual: u32,
    },
    AgentAttemptReasonMismatch {
        node_id: u64,
    },
    ConflictingAgentBinding {
        node_id: u64,
        existing_child_thread_id: String,
        child_thread_id: String,
    },
    TopologyKindMismatch {
        node_id: u64,
        expected: &'static str,
    },
    GroupDefinitionMismatch {
        group_id: u64,
    },
    ActiveChildAtGroupEnd {
        group_id: u64,
        node_id: u64,
    },
    NodeAlreadyCompleted {
        node_id: u64,
    },
    ActiveTopologyAtRunEnd {
        node_id: u64,
    },
    NonTerminalStatus {
        node_id: Option<u64>,
        status: AgentStatus,
    },
    CounterRegression {
        node_id: u64,
        field: &'static str,
        previous: i64,
        next: i64,
    },
    NegativeTokenUsage {
        node_id: u64,
        field: &'static str,
        value: i64,
    },
    NegativeBudget {
        spent: i64,
        total: Option<i64>,
    },
    AggregateOverflow {
        field: &'static str,
    },
}

impl fmt::Display for WorkflowModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid workflow event stream: {self:?}")
    }
}

impl std::error::Error for WorkflowModelError {}
