use std::fmt;

use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;

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

/// Rolled-up counters for a workflow run or one of its phases.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkflowAggregate {
    pub(super) group_count: u64,
    pub(super) agent_count: u64,
    pub(super) active_agent_count: u64,
    pub(super) completed_agent_count: u64,
    pub(super) returned_null_count: u64,
    pub(super) token_usage: TokenUsage,
    pub(super) tool_call_count: u64,
    pub(super) duration_ms: u64,
}

impl WorkflowAggregate {
    pub fn group_count(&self) -> u64 {
        self.group_count
    }

    pub fn agent_count(&self) -> u64 {
        self.agent_count
    }

    pub fn active_agent_count(&self) -> u64 {
        self.active_agent_count
    }

    pub fn completed_agent_count(&self) -> u64 {
        self.completed_agent_count
    }

    pub fn returned_null_count(&self) -> u64 {
        self.returned_null_count
    }

    pub fn token_usage(&self) -> &TokenUsage {
        &self.token_usage
    }

    pub fn tool_call_count(&self) -> u64 {
        self.tool_call_count
    }

    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
}

/// One phase in the declared and dynamically extended phase order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowPhase {
    pub(super) index: u64,
    pub(super) title: String,
    pub(super) state: WorkflowPhaseState,
    /// True only for the synthetic phase used when a script declares and calls no phases.
    pub(super) implicit: bool,
    /// Topology roots in deterministic source order.
    pub(super) root_node_ids: Vec<u64>,
    pub(super) aggregate: WorkflowAggregate,
}

impl WorkflowPhase {
    pub fn index(&self) -> u64 {
        self.index
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn state(&self) -> WorkflowPhaseState {
        self.state
    }

    pub fn is_implicit(&self) -> bool {
        self.implicit
    }

    pub fn root_node_ids(&self) -> &[u64] {
        &self.root_node_ids
    }

    pub fn aggregate(&self) -> &WorkflowAggregate {
        &self.aggregate
    }
}

/// A malformed or out-of-order event that cannot safely update a run projection.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
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
    NoActivePhase,
    ActiveTopologyAtPhaseBoundary {
        phase_index: u64,
        node_id: u64,
    },
    PhaseLimitExceeded {
        maximum: u64,
    },
    LogLimitExceeded {
        maximum: u64,
    },
    EmptyText {
        field: &'static str,
    },
    TextTooLong {
        field: &'static str,
        maximum_bytes: usize,
        actual_bytes: usize,
    },
    TooManyDeclaredPhases {
        maximum: usize,
        actual: usize,
    },
    UnexpectedTopologyId {
        expected: u64,
        actual: u64,
    },
    TopologyLimitExceeded {
        maximum: u64,
    },
    DuplicateTopologyId {
        node_id: u64,
    },
    MissingParent {
        node_id: u64,
        parent_node_id: u64,
    },
    ParentNotActive {
        node_id: u64,
        parent_node_id: u64,
    },
    ParentPhaseMismatch {
        node_id: u64,
        parent_node_id: u64,
        phase_index: u64,
        parent_phase_index: u64,
    },
    AgentPhaseMismatch {
        node_id: u64,
        expected: String,
        actual: String,
    },
    UnknownAgent {
        node_id: u64,
    },
    TopologyKindMismatch {
        node_id: u64,
        expected: &'static str,
    },
    UnexpectedAgentAttempt {
        node_id: u64,
        expected: u32,
        actual: u32,
    },
    AgentRetryLimitExceeded {
        node_id: u64,
        maximum: u32,
    },
    AgentAttemptReasonMismatch {
        node_id: u64,
    },
    AgentDefinitionMismatch {
        node_id: u64,
    },
    AgentNotBound {
        node_id: u64,
    },
    NegativeTokenUsage {
        node_id: u64,
        field: &'static str,
        value: i64,
    },
    TokenCounterRegression {
        node_id: u64,
        field: &'static str,
        previous: i64,
        actual: i64,
    },
    UnsignedCounterRegression {
        node_id: u64,
        field: &'static str,
        previous: u64,
        actual: u64,
    },
    AggregateOverflow {
        field: &'static str,
    },
    AggregateUnderflow {
        field: &'static str,
    },
    NonTerminalStatus {
        node_id: Option<u64>,
        status: AgentStatus,
    },
    ActiveChildAtAgentEnd {
        node_id: u64,
        child_node_id: u64,
    },
    InvalidChildThreadId {
        node_id: u64,
    },
    ConflictingAgentBinding {
        node_id: u64,
        existing_child_thread_id: ThreadId,
        child_thread_id: ThreadId,
    },
    ChildThreadAlreadyBound {
        node_id: u64,
        existing_node_id: u64,
        child_thread_id: ThreadId,
    },
    UnknownGroup {
        group_id: u64,
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
}

impl fmt::Display for WorkflowModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExpectedRunBegin => {
                formatter.write_str("workflow model must start with run_begin")
            }
            Self::DuplicateRunBegin => formatter.write_str("workflow run already began"),
            Self::RunIdMismatch { expected, actual } => write!(
                formatter,
                "workflow event belongs to run {actual}; expected {expected}"
            ),
            Self::RunAlreadyCompleted => formatter.write_str("workflow run already completed"),
            Self::PhaseIndexOverflow => formatter.write_str("workflow phase index overflow"),
            Self::UnexpectedPhaseIndex { expected, actual } => write!(
                formatter,
                "workflow phase index is {actual}; expected {expected}"
            ),
            Self::PhaseTitleMismatch {
                phase_index,
                expected,
                actual,
            } => write!(
                formatter,
                "workflow phase {phase_index} is titled {actual}; expected {expected}"
            ),
            Self::PhaseAlreadyActive { phase_index } => {
                write!(formatter, "workflow phase {phase_index} is already active")
            }
            Self::PhaseNotActive { phase_index } => {
                write!(formatter, "workflow phase {phase_index} is not active")
            }
            Self::NoActivePhase => formatter.write_str("workflow has no active phase"),
            Self::PhaseLimitExceeded { maximum } => {
                write!(
                    formatter,
                    "workflow phase limit exceeded; maximum is {maximum}"
                )
            }
            Self::LogLimitExceeded { maximum } => {
                write!(
                    formatter,
                    "workflow log limit exceeded; maximum is {maximum}"
                )
            }
            Self::EmptyText { field } => write!(formatter, "workflow {field} must not be empty"),
            Self::TextTooLong {
                field,
                maximum_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "workflow {field} is {actual_bytes} bytes; maximum is {maximum_bytes}"
            ),
            Self::TooManyDeclaredPhases { maximum, actual } => write!(
                formatter,
                "workflow declares {actual} phases; maximum is {maximum}"
            ),
            Self::ActiveTopologyAtPhaseBoundary { .. }
            | Self::UnexpectedTopologyId { .. }
            | Self::TopologyLimitExceeded { .. }
            | Self::DuplicateTopologyId { .. }
            | Self::MissingParent { .. }
            | Self::ParentNotActive { .. }
            | Self::ParentPhaseMismatch { .. }
            | Self::AgentPhaseMismatch { .. }
            | Self::UnknownAgent { .. }
            | Self::TopologyKindMismatch { .. }
            | Self::UnexpectedAgentAttempt { .. }
            | Self::AgentRetryLimitExceeded { .. }
            | Self::AgentAttemptReasonMismatch { .. }
            | Self::AgentDefinitionMismatch { .. }
            | Self::AgentNotBound { .. }
            | Self::NegativeTokenUsage { .. }
            | Self::TokenCounterRegression { .. }
            | Self::UnsignedCounterRegression { .. }
            | Self::AggregateOverflow { .. }
            | Self::AggregateUnderflow { .. }
            | Self::NonTerminalStatus { .. }
            | Self::ActiveChildAtAgentEnd { .. }
            | Self::InvalidChildThreadId { .. }
            | Self::ConflictingAgentBinding { .. }
            | Self::ChildThreadAlreadyBound { .. }
            | Self::UnknownGroup { .. }
            | Self::GroupDefinitionMismatch { .. }
            | Self::ActiveChildAtGroupEnd { .. }
            | Self::NodeAlreadyCompleted { .. } => {
                write!(formatter, "invalid workflow topology event: {self:?}")
            }
        }
    }
}

impl std::error::Error for WorkflowModelError {}
