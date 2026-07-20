//! Event-clock timing projection for the live workflow monitor.

use super::MAX_PHASES_PER_RUN;
use codex_core_workflows::WorkflowPhaseState;
use codex_core_workflows::WorkflowRunModel;
use codex_core_workflows::WorkflowRunState;
use codex_protocol::protocol::WorkflowEvent;

/// Bounded run and phase timing derived only from app-server event timestamps.
///
/// Active values are lower bounds because no wall-clock time is invented between events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkflowRunTiming {
    started_at: Option<i64>,
    last_observed_at: Option<i64>,
    completed_at: Option<i64>,
    phases: Vec<WorkflowPhaseTiming>,
    active_phase_index: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct WorkflowPhaseTiming {
    started_at: Option<i64>,
    completed_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimingBoundary {
    Progress,
    PhaseStarted(u64),
    PhaseCompleted(u64),
    RunCompleted,
}

impl WorkflowRunTiming {
    pub(super) fn from_run_begin(model: &WorkflowRunModel, observed_at: Option<i64>) -> Self {
        let active_phase_index = model
            .phases
            .iter()
            .position(|phase| phase.state == WorkflowPhaseState::Active);
        Self::new(model.phases.len(), active_phase_index, observed_at)
    }

    fn new(phase_count: usize, active_phase_index: Option<usize>, started_at: Option<i64>) -> Self {
        let phase_count = phase_count.min(MAX_PHASES_PER_RUN);
        let active_phase_index = active_phase_index.filter(|index| *index < phase_count);
        let mut phases = vec![WorkflowPhaseTiming::default(); phase_count];
        if let Some(index) = active_phase_index {
            phases[index].started_at = started_at;
        }
        Self {
            started_at,
            last_observed_at: started_at,
            completed_at: None,
            phases,
            active_phase_index,
        }
    }

    pub(super) fn observe_event(&mut self, event: &WorkflowEvent, observed_at: Option<i64>) {
        let boundary = match event {
            WorkflowEvent::PhaseBegin(event) => TimingBoundary::PhaseStarted(event.phase_index),
            WorkflowEvent::PhaseEnd(event) => TimingBoundary::PhaseCompleted(event.phase_index),
            WorkflowEvent::RunEnd(_) => TimingBoundary::RunCompleted,
            WorkflowEvent::RunBegin(_)
            | WorkflowEvent::GroupBegin(_)
            | WorkflowEvent::GroupEnd(_)
            | WorkflowEvent::AgentBegin(_)
            | WorkflowEvent::AgentBound(_)
            | WorkflowEvent::AgentUpdated(_)
            | WorkflowEvent::AgentEnd(_)
            | WorkflowEvent::Log(_) => TimingBoundary::Progress,
        };
        self.observe(boundary, observed_at);
    }

    fn observe(&mut self, boundary: TimingBoundary, observed_at: Option<i64>) {
        if let Some(observed_at) = observed_at {
            self.last_observed_at = Some(
                self.last_observed_at
                    .map_or(observed_at, |previous| previous.max(observed_at)),
            );
        }

        match boundary {
            TimingBoundary::Progress => {}
            TimingBoundary::PhaseStarted(phase_index) => {
                let Some(index) = self.ensure_phase(phase_index) else {
                    return;
                };
                self.active_phase_index = Some(index);
                if let Some(observed_at) = observed_at {
                    self.phases[index].started_at.get_or_insert(observed_at);
                }
            }
            TimingBoundary::PhaseCompleted(phase_index) => {
                let Some(index) = self.ensure_phase(phase_index) else {
                    return;
                };
                if let Some(observed_at) = observed_at {
                    self.phases[index].completed_at.get_or_insert(observed_at);
                }
                if self.active_phase_index == Some(index) {
                    self.active_phase_index = None;
                }
            }
            TimingBoundary::RunCompleted => {
                if let Some(observed_at) = observed_at {
                    self.completed_at.get_or_insert(observed_at);
                    if let Some(index) = self.active_phase_index {
                        self.phases[index].completed_at.get_or_insert(observed_at);
                    }
                }
                self.active_phase_index = None;
            }
        }
    }

    fn ensure_phase(&mut self, phase_index: u64) -> Option<usize> {
        let index = usize::try_from(phase_index).ok()?;
        if index >= MAX_PHASES_PER_RUN {
            return None;
        }
        if self.phases.len() <= index {
            self.phases
                .resize_with(index.saturating_add(1), WorkflowPhaseTiming::default);
        }
        Some(index)
    }

    pub(super) fn run_display(&self, state: WorkflowRunState) -> Option<String> {
        let (label, end) = match state {
            WorkflowRunState::Running => ("elapsed ≥", self.last_observed_at),
            WorkflowRunState::Completed => ("duration", self.completed_at),
        };
        elapsed_seconds(self.started_at, end)
            .map(|seconds| format!("{label} {}", format_seconds(seconds)))
    }

    pub(super) fn phase_display(
        &self,
        phase_index: u64,
        state: WorkflowPhaseState,
    ) -> Option<String> {
        let index = usize::try_from(phase_index).ok()?;
        let phase = self.phases.get(index)?;
        let (label, end) = match state {
            WorkflowPhaseState::Pending => return None,
            WorkflowPhaseState::Active => ("elapsed ≥", self.last_observed_at),
            WorkflowPhaseState::Completed => ("duration", phase.completed_at),
        };
        elapsed_seconds(phase.started_at, end)
            .map(|seconds| format!("{label} {}", format_seconds(seconds)))
    }
}

fn elapsed_seconds(started_at: Option<i64>, ended_at: Option<i64>) -> Option<u64> {
    let elapsed = ended_at?.saturating_sub(started_at?).max(0);
    u64::try_from(elapsed).ok()
}

fn format_seconds(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
#[path = "timing_tests.rs"]
mod tests;
