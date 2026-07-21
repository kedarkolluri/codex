use std::fmt;

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

/// One phase in the declared and dynamically extended phase order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowPhase {
    pub(super) index: u64,
    pub(super) title: String,
    pub(super) state: WorkflowPhaseState,
    /// True only for the synthetic phase used when a script declares and calls no phases.
    pub(super) implicit: bool,
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
        }
    }
}

impl std::error::Error for WorkflowModelError {}
