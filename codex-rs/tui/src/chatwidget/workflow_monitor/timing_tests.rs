use super::*;
use pretty_assertions::assert_eq;

#[test]
fn missing_timestamps_do_not_invent_elapsed_values() {
    let mut timing = WorkflowRunTiming::new(
        /*phase_count*/ 1,
        /*active_phase_index*/ Some(0),
        /*started_at*/ None,
    );

    timing.observe(TimingBoundary::Progress, /*observed_at*/ Some(50));
    timing.observe(
        TimingBoundary::PhaseCompleted(/*phase_index*/ 0),
        /*observed_at*/ Some(70),
    );
    timing.observe(TimingBoundary::RunCompleted, /*observed_at*/ Some(80));

    assert_eq!(
        timing,
        WorkflowRunTiming {
            started_at: None,
            last_observed_at: Some(80),
            completed_at: Some(80),
            phases: vec![WorkflowPhaseTiming {
                started_at: None,
                completed_at: Some(70),
            }],
            active_phase_index: None,
        }
    );
    assert_eq!(timing.run_display(WorkflowRunState::Completed), None);
    assert_eq!(
        timing.phase_display(/*phase_index*/ 0, WorkflowPhaseState::Completed),
        None
    );
}

#[test]
fn out_of_order_timestamps_are_monotonic_and_non_negative() {
    let mut timing = WorkflowRunTiming::new(
        /*phase_count*/ 1,
        /*active_phase_index*/ None,
        /*started_at*/ Some(100),
    );

    timing.observe(
        TimingBoundary::PhaseStarted(/*phase_index*/ 0),
        /*observed_at*/ Some(120),
    );
    timing.observe(TimingBoundary::Progress, /*observed_at*/ Some(110));
    timing.observe(
        TimingBoundary::PhaseCompleted(/*phase_index*/ 0),
        /*observed_at*/ Some(90),
    );
    timing.observe(TimingBoundary::RunCompleted, /*observed_at*/ Some(80));

    assert_eq!(timing.last_observed_at, Some(120));
    assert_eq!(
        timing.phase_display(/*phase_index*/ 0, WorkflowPhaseState::Completed),
        Some("duration 0s".to_string())
    );
    assert_eq!(
        timing.run_display(WorkflowRunState::Completed),
        Some("duration 0s".to_string())
    );
}

#[test]
fn duplicate_boundaries_keep_the_first_boundary_timestamp() {
    let mut timing = WorkflowRunTiming::new(
        /*phase_count*/ 1,
        /*active_phase_index*/ None,
        /*started_at*/ Some(10),
    );

    timing.observe(
        TimingBoundary::PhaseStarted(/*phase_index*/ 0),
        /*observed_at*/ Some(20),
    );
    timing.observe(
        TimingBoundary::PhaseStarted(/*phase_index*/ 0),
        /*observed_at*/ Some(25),
    );
    timing.observe(
        TimingBoundary::PhaseCompleted(/*phase_index*/ 0),
        /*observed_at*/ Some(30),
    );
    timing.observe(
        TimingBoundary::PhaseCompleted(/*phase_index*/ 0),
        /*observed_at*/ Some(40),
    );

    assert_eq!(
        timing,
        WorkflowRunTiming {
            started_at: Some(10),
            last_observed_at: Some(40),
            completed_at: None,
            phases: vec![WorkflowPhaseTiming {
                started_at: Some(20),
                completed_at: Some(30),
            }],
            active_phase_index: None,
        }
    );
    assert_eq!(
        timing.phase_display(/*phase_index*/ 0, WorkflowPhaseState::Completed),
        Some("duration 10s".to_string())
    );
}

#[test]
fn phase_timing_storage_is_bounded() {
    let mut timing = WorkflowRunTiming::new(
        MAX_PHASES_PER_RUN.saturating_add(10),
        /*active_phase_index*/ Some(MAX_PHASES_PER_RUN),
        /*started_at*/ Some(1),
    );

    timing.observe(
        TimingBoundary::PhaseStarted(MAX_PHASES_PER_RUN as u64),
        /*observed_at*/ Some(2),
    );

    assert_eq!(timing.phases.len(), MAX_PHASES_PER_RUN);
    assert_eq!(timing.active_phase_index, None);
}

#[test]
fn elapsed_format_is_compact_and_stable() {
    assert_eq!(format_seconds(/*seconds*/ 0), "0s");
    assert_eq!(format_seconds(/*seconds*/ 59), "59s");
    assert_eq!(format_seconds(/*seconds*/ 65), "1m 05s");
    assert_eq!(format_seconds(/*seconds*/ 3_661), "1h 01m 01s");
}

#[test]
fn active_timings_are_event_clock_lower_bounds() {
    let mut timing = WorkflowRunTiming::new(
        /*phase_count*/ 1,
        /*active_phase_index*/ Some(0),
        /*started_at*/ Some(100),
    );
    timing.observe(TimingBoundary::Progress, /*observed_at*/ Some(165));

    assert_eq!(
        timing.run_display(WorkflowRunState::Running),
        Some("elapsed ≥ 1m 05s".to_string())
    );
    assert_eq!(
        timing.phase_display(/*phase_index*/ 0, WorkflowPhaseState::Active),
        Some("elapsed ≥ 1m 05s".to_string())
    );
}
