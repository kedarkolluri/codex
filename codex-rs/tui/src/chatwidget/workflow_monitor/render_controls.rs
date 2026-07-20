//! Bounded status rendering for workflow run and selected-agent controls.

use super::MonitoredRun;
use super::agent_control::WorkflowAgentControlRequestState;
use super::pause::WorkflowPauseRequestState;
use super::pause::WorkflowResumeRequestState;
use super::render::compact_run_id;
use super::render::push_line;
use super::save::WorkflowSaveRequestState;
use super::save::workflow_save_scope_label;
use super::stop::WorkflowStopRequestState;
use crate::app_event::WorkflowSaveIntent;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

pub(super) fn render_run_controls(lines: &mut Vec<Line<'static>>, run: &MonitoredRun, width: u16) {
    match &run.stop_request {
        WorkflowStopRequestState::Idle => {}
        WorkflowStopRequestState::Pending { .. } => push_line(
            lines,
            vec!["  stop  ".bold(), "requesting cancellation…".cyan()].into(),
            width,
            "    ",
        ),
        WorkflowStopRequestState::Applied => push_line(
            lines,
            vec!["  stop  ".bold(), "cancellation applied".green()].into(),
            width,
            "    ",
        ),
        WorkflowStopRequestState::AlreadyRequested => push_line(
            lines,
            vec!["  stop  ".bold(), "workflow was already stopped".dim()].into(),
            width,
            "    ",
        ),
        WorkflowStopRequestState::Failed(error) => push_line(
            lines,
            vec!["  stop failed  ".bold().red(), error.clone().red()].into(),
            width,
            "    ",
        ),
    }

    match &run.pause_request {
        WorkflowPauseRequestState::Idle => {}
        WorkflowPauseRequestState::Pending { .. } => push_line(
            lines,
            vec![
                "  pause  ".bold(),
                "publishing checkpoint and stopping…".cyan(),
            ]
            .into(),
            width,
            "    ",
        ),
        WorkflowPauseRequestState::Applied => push_line(
            lines,
            vec!["  pause  ".bold(), "durable checkpoint published".green()].into(),
            width,
            "    ",
        ),
        WorkflowPauseRequestState::AlreadyRequested => push_line(
            lines,
            vec!["  pause  ".bold(), "workflow was already paused".dim()].into(),
            width,
            "    ",
        ),
        WorkflowPauseRequestState::Failed(error) => push_line(
            lines,
            vec!["  pause failed  ".bold().red(), error.clone().red()].into(),
            width,
            "    ",
        ),
    }

    match &run.resume_request {
        WorkflowResumeRequestState::Idle => {}
        WorkflowResumeRequestState::Pending { .. } => push_line(
            lines,
            vec!["  resume  ".bold(), "admitting durable successor…".cyan()].into(),
            width,
            "    ",
        ),
        WorkflowResumeRequestState::Succeeded { successor_run_id } => push_line(
            lines,
            vec![
                "  resumed  ".bold(),
                format!("successor {}", compact_run_id(successor_run_id)).green(),
            ]
            .into(),
            width,
            "    ",
        ),
        WorkflowResumeRequestState::Failed(error) => push_line(
            lines,
            vec!["  resume failed  ".bold().red(), error.clone().red()].into(),
            width,
            "    ",
        ),
    }

    match &run.save_request {
        WorkflowSaveRequestState::Idle => {}
        WorkflowSaveRequestState::Pending(request) => {
            let action = match request.intent {
                WorkflowSaveIntent::Create => "creating",
                WorkflowSaveIntent::Overwrite => "overwriting",
            };
            push_line(
                lines,
                vec![
                    "  save  ".bold(),
                    format!(
                        "{action} exact {} script…",
                        workflow_save_scope_label(request.target.scope)
                    )
                    .cyan(),
                ]
                .into(),
                width,
                "    ",
            );
        }
        WorkflowSaveRequestState::Conflict(target) => push_line(
            lines,
            vec![
                "  save conflict  ".bold().red(),
                format!(
                    "{} workflow already exists",
                    workflow_save_scope_label(target.scope)
                )
                .red(),
            ]
            .into(),
            width,
            "    ",
        ),
        WorkflowSaveRequestState::Created(target) => push_line(
            lines,
            vec![
                "  saved  ".bold(),
                format!(
                    "exact {} script created",
                    workflow_save_scope_label(target.scope)
                )
                .green(),
            ]
            .into(),
            width,
            "    ",
        ),
        WorkflowSaveRequestState::Overwritten(target) => push_line(
            lines,
            vec![
                "  saved  ".bold(),
                format!(
                    "exact {} script overwritten",
                    workflow_save_scope_label(target.scope)
                )
                .green(),
            ]
            .into(),
            width,
            "    ",
        ),
        WorkflowSaveRequestState::Failed { target, error } => push_line(
            lines,
            vec![
                "  save failed  ".bold().red(),
                format!("{} · {error}", workflow_save_scope_label(target.scope)).red(),
            ]
            .into(),
            width,
            "    ",
        ),
    }
}

pub(super) fn agent_control_status(control: &WorkflowAgentControlRequestState) -> Span<'static> {
    match control {
        WorkflowAgentControlRequestState::Pending(request) => match request.action {
            codex_app_server_protocol::WorkflowAgentControlAction::Skip => "skipping…".cyan(),
            codex_app_server_protocol::WorkflowAgentControlAction::Retry => "retrying…".cyan(),
        },
        WorkflowAgentControlRequestState::Skipped(target) => {
            format!("attempt {} skipped", target.attempt.saturating_add(1)).green()
        }
        WorkflowAgentControlRequestState::RetryScheduled { target, attempt } => format!(
            "attempt {} → {}",
            target.attempt.saturating_add(1),
            attempt.saturating_add(1)
        )
        .green(),
        WorkflowAgentControlRequestState::RetryLimitReached(target) => {
            format!("attempt {} retry limit", target.attempt.saturating_add(1)).red()
        }
        WorkflowAgentControlRequestState::Failed { target, error } => format!(
            "attempt {} control failed: {error}",
            target.attempt.saturating_add(1)
        )
        .red(),
    }
}
