//! Renderer-neutral projection of the stable workflow progress event stream.

use codex_code_mode_protocol::WORKFLOW_NAME_MAX_BYTES;
use codex_code_mode_protocol::WORKFLOW_PHASE_MAX_EVENTS;
use codex_code_mode_protocol::WORKFLOW_PHASE_TITLE_MAX_BYTES;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;

mod types;

pub use types::WorkflowModelError;
pub use types::WorkflowPhase;
pub use types::WorkflowPhaseState;
pub use types::WorkflowRunState;

const IMPLICIT_ROOT_PHASE_TITLE: &str = "root";
const WORKFLOW_RUN_ID_MAX_BYTES: usize = 256;
const WORKFLOW_ARGS_DIGEST_MAX_BYTES: usize = 256;

/// Renderer-neutral state derived from one workflow run's ordered progress events.
///
/// Construct the initial deterministic state from the run's `RunBegin` event. Later Stage 05
/// slices add transactional reduction of the remaining workflow event family without changing
/// this state representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRunModel {
    run_id: String,
    resumed_from_run_id: Option<String>,
    name: String,
    args_digest: String,
    state: WorkflowRunState,
    status: AgentStatus,
    terminal_reason: Option<WorkflowRunTerminalReason>,
    phases: Vec<WorkflowPhase>,
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

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn resumed_from_run_id(&self) -> Option<&str> {
        self.resumed_from_run_id.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn args_digest(&self) -> &str {
        &self.args_digest
    }

    pub fn state(&self) -> WorkflowRunState {
        self.state
    }

    pub fn status(&self) -> &AgentStatus {
        &self.status
    }

    pub fn terminal_reason(&self) -> Option<WorkflowRunTerminalReason> {
        self.terminal_reason
    }

    pub fn phases(&self) -> &[WorkflowPhase] {
        &self.phases
    }

    fn from_run_begin(event: &WorkflowRunBeginEvent) -> Result<Self, WorkflowModelError> {
        validate_text("run_id", &event.run_id, WORKFLOW_RUN_ID_MAX_BYTES)?;
        if let Some(resumed_from_run_id) = &event.resumed_from_run_id {
            validate_text(
                "resumed_from_run_id",
                resumed_from_run_id,
                WORKFLOW_RUN_ID_MAX_BYTES,
            )?;
        }
        validate_text("name", &event.name, WORKFLOW_NAME_MAX_BYTES)?;
        validate_text(
            "args_digest",
            &event.args_digest,
            WORKFLOW_ARGS_DIGEST_MAX_BYTES,
        )?;
        let phase_limit = usize::try_from(WORKFLOW_PHASE_MAX_EVENTS).unwrap_or(usize::MAX);
        if event.phases.len() > phase_limit {
            return Err(WorkflowModelError::TooManyDeclaredPhases {
                maximum: phase_limit,
                actual: event.phases.len(),
            });
        }
        for title in &event.phases {
            validate_text("phase title", title, WORKFLOW_PHASE_TITLE_MAX_BYTES)?;
        }

        let phases = if event.phases.is_empty() {
            vec![WorkflowPhase {
                index: 0,
                title: IMPLICIT_ROOT_PHASE_TITLE.to_string(),
                state: WorkflowPhaseState::Active,
                implicit: true,
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
            active_phase_index,
            next_phase_index: 0,
        })
    }
}

fn validate_text(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
) -> Result<(), WorkflowModelError> {
    if value.trim().is_empty() {
        Err(WorkflowModelError::EmptyText { field })
    } else if value.len() > maximum_bytes {
        Err(WorkflowModelError::TextTooLong {
            field,
            maximum_bytes,
            actual_bytes: value.len(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "run_model_tests.rs"]
mod tests;
