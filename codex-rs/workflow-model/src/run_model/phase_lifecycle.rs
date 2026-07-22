use codex_protocol::protocol::WorkflowLogEvent;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;

use super::*;

impl WorkflowRunModel {
    pub(super) fn reduce_phase_begin(
        &mut self,
        event: &WorkflowPhaseBeginEvent,
    ) -> Result<(), WorkflowModelError> {
        validate_text("phase title", &event.title, WORKFLOW_PHASE_TITLE_MAX_BYTES)?;

        if let Some(active_phase_index) = self.active_phase_index {
            let position = usize::try_from(active_phase_index)
                .map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
            let phase = self
                .phases
                .get(position)
                .ok_or(WorkflowModelError::PhaseNotActive {
                    phase_index: active_phase_index,
                })?;
            if phase.implicit && self.next_phase_index == 0 && event.phase_index == 0 {
                self.ensure_phase_topology_inactive(active_phase_index)?;
                let next_phase_index = self
                    .next_phase_index
                    .checked_add(1)
                    .ok_or(WorkflowModelError::PhaseIndexOverflow)?;
                let phase =
                    self.phases
                        .get_mut(position)
                        .ok_or(WorkflowModelError::PhaseNotActive {
                            phase_index: active_phase_index,
                        })?;
                phase.title.clone_from(&event.title);
                phase.implicit = false;
                self.next_phase_index = next_phase_index;
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
        if event.phase_index >= WORKFLOW_PHASE_MAX_EVENTS {
            return Err(WorkflowModelError::PhaseLimitExceeded {
                maximum: WORKFLOW_PHASE_MAX_EVENTS,
            });
        }

        let next_phase_index = self
            .next_phase_index
            .checked_add(1)
            .ok_or(WorkflowModelError::PhaseIndexOverflow)?;
        let position = usize::try_from(event.phase_index)
            .map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
        if position < self.phases.len() {
            let phase = &self.phases[position];
            if phase.title != event.title {
                return Err(WorkflowModelError::PhaseTitleMismatch {
                    phase_index: event.phase_index,
                    expected: phase.title.clone(),
                    actual: event.title.clone(),
                });
            }
            debug_assert_eq!(phase.state, WorkflowPhaseState::Pending);
            self.phases[position].state = WorkflowPhaseState::Active;
        } else {
            debug_assert_eq!(position, self.phases.len());
            self.phases.push(WorkflowPhase {
                index: event.phase_index,
                title: event.title.clone(),
                state: WorkflowPhaseState::Active,
                implicit: false,
                root_node_ids: Vec::new(),
                aggregate: WorkflowAggregate::default(),
            });
        }
        self.active_phase_index = Some(event.phase_index);
        self.next_phase_index = next_phase_index;
        Ok(())
    }

    pub(super) fn reduce_phase_end(
        &mut self,
        event: &WorkflowPhaseEndEvent,
    ) -> Result<(), WorkflowModelError> {
        validate_text("phase title", &event.title, WORKFLOW_PHASE_TITLE_MAX_BYTES)?;
        if self.active_phase_index != Some(event.phase_index) {
            return Err(WorkflowModelError::PhaseNotActive {
                phase_index: event.phase_index,
            });
        }
        let position = usize::try_from(event.phase_index)
            .map_err(|_| WorkflowModelError::PhaseIndexOverflow)?;
        let phase = self
            .phases
            .get(position)
            .ok_or(WorkflowModelError::PhaseNotActive {
                phase_index: event.phase_index,
            })?;
        if phase.implicit {
            return Err(WorkflowModelError::PhaseNotActive {
                phase_index: event.phase_index,
            });
        }
        if phase.title != event.title {
            return Err(WorkflowModelError::PhaseTitleMismatch {
                phase_index: event.phase_index,
                expected: phase.title.clone(),
                actual: event.title.clone(),
            });
        }
        self.ensure_phase_topology_inactive(event.phase_index)?;
        debug_assert_eq!(phase.state, WorkflowPhaseState::Active);

        self.phases[position].state = WorkflowPhaseState::Completed;
        self.active_phase_index = None;
        Ok(())
    }

    pub(super) fn reduce_log(
        &mut self,
        event: &WorkflowLogEvent,
    ) -> Result<(), WorkflowModelError> {
        validate_text(
            "log message",
            &event.message,
            WORKFLOW_LOG_MESSAGE_MAX_BYTES,
        )?;
        if self.log_event_count >= WORKFLOW_LOG_MAX_EVENTS {
            return Err(WorkflowModelError::LogLimitExceeded {
                maximum: WORKFLOW_LOG_MAX_EVENTS,
            });
        }
        self.log_event_count =
            self.log_event_count
                .checked_add(1)
                .ok_or(WorkflowModelError::LogLimitExceeded {
                    maximum: WORKFLOW_LOG_MAX_EVENTS,
                })?;
        Ok(())
    }
}
