//! Live, bounded workflow progress state for one chat thread.

use super::ChatWidget;
use super::Notification;
use crate::app_event::AppEvent;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowRunStatus;
use codex_core_workflows::WorkflowRunModel;
use codex_core_workflows::WorkflowRunState;
use codex_core_workflows::WorkflowTopologyNode;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use std::collections::BTreeMap;
use std::collections::VecDeque;

mod adapter;
mod agent_control;
mod navigation;
mod pause;
mod render;
mod render_controls;
mod save;
mod stop;
mod timing;

pub(super) use adapter::WorkflowNotification;
use agent_control::WorkflowAgentControlRequestState;
use navigation::WorkflowMonitorSelection;
use pause::WorkflowPauseRequestState;
use pause::WorkflowResumeRequestState;
#[cfg(test)]
use save::MAX_SAVE_ERROR_CHARS;
use save::WorkflowSaveRequestState;
use stop::WorkflowStopRequestState;
use timing::WorkflowRunTiming;

const MAX_ACTIVE_RUNS: usize = 4;
const MAX_COMPLETED_RUNS: usize = 2;
const MAX_SUMMARIZED_RUNS: usize = 4;
const MAX_COMPLETED_SUMMARIZED_RUNS: usize = 2;
const MAX_SATURATED_RUN_IDS: usize = 16;
const MAX_LOGS_PER_RUN: usize = 3;
const MAX_LOG_CHARS: usize = 320;
const MAX_RUN_ID_CHARS: usize = 128;
const MAX_SUMMARY_NAME_CHARS: usize = 96;
const MAX_DURABLE_NAME_BYTES: usize = 256;
const MAX_PENDING_WORKFLOW_STATUS_READS: usize = 8;
const MAX_PHASES_PER_RUN: usize = 64;
const MAX_TOPOLOGY_NODES_PER_RUN: usize = 256;

/// UI-only state retained alongside the renderer-neutral workflow projection.
#[derive(Clone, Debug, Eq, PartialEq)]
struct MonitoredRun {
    model: WorkflowRunModel,
    /// Durable lifecycle fence read after a replayed start. This never mutates the event-derived
    /// topology; any subsequently accepted workflow event clears it and becomes authoritative.
    reconciled_status: Option<WorkflowRunStatus>,
    /// Keeps an unanswered durable read non-controllable without treating it as terminal storage.
    status_read_pending: bool,
    /// Exact app-server identity retained separately from the bounded display projection.
    identity: WorkflowRunIdentity,
    logs: VecDeque<String>,
    save_request: WorkflowSaveRequestState,
    stop_request: WorkflowStopRequestState,
    pause_request: WorkflowPauseRequestState,
    resume_request: WorkflowResumeRequestState,
    agent_control_requests: BTreeMap<u64, WorkflowAgentControlRequestState>,
    timing: WorkflowRunTiming,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkflowRunIdentity {
    thread_id: Option<ThreadId>,
    durable_name: Option<String>,
}

/// Lightweight state for a run that cannot retain a full topology projection.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SummarizedRun {
    run_id: String,
    name: String,
    status: SummarizedRunStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SummarizedRunStatus {
    Starting,
    Running,
    Reconciling,
    Reconciled(WorkflowRunStatus),
    Completed {
        status: AgentStatus,
        terminal_reason: Option<WorkflowRunTerminalReason>,
    },
}

impl SummarizedRunStatus {
    fn is_completed(&self) -> bool {
        matches!(self, Self::Reconciled(_) | Self::Completed { .. })
    }
}

struct WorkflowCompletionContext {
    name: String,
    agent_count: Option<usize>,
}

/// Tracks active runs plus a small tail of completed runs for the owning thread.
#[derive(Debug, Default)]
pub(super) struct WorkflowMonitor {
    runs: VecDeque<MonitoredRun>,
    summarized_runs: VecDeque<SummarizedRun>,
    saturated_run_ids: VecDeque<String>,
    saturated_run_count: u64,
    selection: Option<WorkflowMonitorSelection>,
    selected_run_id: Option<String>,
    /// Advances for every accepted event and request so late async results cannot win an ABA race.
    status_revisions: BTreeMap<String, u64>,
    /// At most one in-flight durable-status read, revision-fenced, per tracked run.
    pending_status_reads: BTreeMap<String, u64>,
}

impl MonitoredRun {
    fn is_effectively_running(&self) -> bool {
        self.model.state == WorkflowRunState::Running && self.reconciled_status.is_none()
    }

    fn effective_state(&self) -> WorkflowRunState {
        if self.is_effectively_running() {
            WorkflowRunState::Running
        } else {
            WorkflowRunState::Completed
        }
    }

    fn retention_state(&self) -> WorkflowRunState {
        if self.status_read_pending {
            self.model.state
        } else {
            self.effective_state()
        }
    }

    fn is_effectively_paused(&self) -> bool {
        self.reconciled_status == Some(WorkflowRunStatus::Paused)
            || (self.model.state == WorkflowRunState::Completed
                && self.model.terminal_reason == Some(WorkflowRunTerminalReason::Paused))
    }
}

impl WorkflowMonitor {
    pub(super) fn is_visible(&self) -> bool {
        !self.runs.is_empty() || !self.summarized_runs.is_empty() || self.saturated_run_count > 0
    }

    /// Consumes a workflow notification and returns whether visible state changed.
    ///
    /// Invalid or out-of-order events are ignored. `WorkflowRunModel::apply` is transactional, so
    /// a rejected event cannot partially mutate the projection already on screen.
    fn handle_notification(&mut self, notification: WorkflowNotification) -> bool {
        let observed_at = Some(notification.observed_at());
        let thread_id = ThreadId::from_string(notification.thread_id()).ok();
        let durable_name = notification
            .durable_name()
            .filter(|name| durable_name_is_display_safe(name))
            .map(ToString::to_string);
        let identity = WorkflowRunIdentity {
            thread_id,
            durable_name,
        };
        let event = notification.into_event();
        let run_id = event_run_id(&event).to_string();
        let changed = if matches!(event, WorkflowEvent::RunBegin(_)) {
            self.start_run(event, observed_at, identity)
        } else {
            self.apply_event(event, observed_at)
        };
        if changed {
            self.advance_status_revision_after_event(&run_id);
            self.reconcile_selection();
        }
        changed
    }

    fn apply_event(&mut self, event: WorkflowEvent, observed_at: Option<i64>) -> bool {
        let run_id = event_run_id(&event);
        let Some(run) = self.runs.iter_mut().find(|run| run.model.run_id == run_id) else {
            if let Some(summary) = self
                .summarized_runs
                .iter_mut()
                .find(|summary| summary.run_id == run_id)
            {
                let status = match event {
                    WorkflowEvent::RunEnd(event) => SummarizedRunStatus::Completed {
                        status: event.status,
                        terminal_reason: event.terminal_reason,
                    },
                    WorkflowEvent::RunBegin(_)
                    | WorkflowEvent::PhaseBegin(_)
                    | WorkflowEvent::PhaseEnd(_)
                    | WorkflowEvent::GroupBegin(_)
                    | WorkflowEvent::GroupEnd(_)
                    | WorkflowEvent::AgentBegin(_)
                    | WorkflowEvent::AgentBound(_)
                    | WorkflowEvent::AgentUpdated(_)
                    | WorkflowEvent::AgentEnd(_)
                    | WorkflowEvent::Log(_)
                        if matches!(
                            &summary.status,
                            SummarizedRunStatus::Reconciling | SummarizedRunStatus::Reconciled(_)
                        ) =>
                    {
                        SummarizedRunStatus::Running
                    }
                    WorkflowEvent::RunBegin(_)
                    | WorkflowEvent::PhaseBegin(_)
                    | WorkflowEvent::PhaseEnd(_)
                    | WorkflowEvent::GroupBegin(_)
                    | WorkflowEvent::GroupEnd(_)
                    | WorkflowEvent::AgentBegin(_)
                    | WorkflowEvent::AgentBound(_)
                    | WorkflowEvent::AgentUpdated(_)
                    | WorkflowEvent::AgentEnd(_)
                    | WorkflowEvent::Log(_) => return false,
                };
                if summary.status == status {
                    return false;
                }
                summary.status = status;
                self.prune_completed_summarized_runs();
                return true;
            }
            let saturated_index = self
                .saturated_run_ids
                .iter()
                .position(|saturated_run_id| saturated_run_id == run_id);
            if let Some(index) = saturated_index {
                return matches!(event, WorkflowEvent::RunEnd(_))
                    && self.remove_saturated_run(Some(index));
            }
            if matches!(event, WorkflowEvent::RunEnd(_)) {
                let tracked_count = u64::try_from(self.saturated_run_ids.len()).unwrap_or(u64::MAX);
                if self.saturated_run_count > tracked_count {
                    self.saturated_run_count = self.saturated_run_count.saturating_sub(1);
                    return true;
                }
                if self.saturated_run_count > 0 {
                    return false;
                }
            }
            tracing::warn!(run_id, "ignoring workflow event for an unknown run");
            return false;
        };
        if matches!(
            &event,
            WorkflowEvent::GroupBegin(_) | WorkflowEvent::AgentBegin(_)
        ) && run.model.topology.len() >= MAX_TOPOLOGY_NODES_PER_RUN
        {
            tracing::warn!(
                run_id,
                "ignoring workflow node because the live monitor is full"
            );
            return false;
        }
        if let WorkflowEvent::PhaseBegin(event) = &event
            && usize::try_from(event.phase_index)
                .map_or(/*default*/ true, |index| index >= MAX_PHASES_PER_RUN)
        {
            tracing::warn!(
                run_id,
                "ignoring workflow phase because the live monitor is full"
            );
            return false;
        }

        let log = match &event {
            WorkflowEvent::Log(event) => bounded_log(&event.message),
            WorkflowEvent::RunBegin(_)
            | WorkflowEvent::RunEnd(_)
            | WorkflowEvent::PhaseBegin(_)
            | WorkflowEvent::PhaseEnd(_)
            | WorkflowEvent::GroupBegin(_)
            | WorkflowEvent::GroupEnd(_)
            | WorkflowEvent::AgentBegin(_)
            | WorkflowEvent::AgentBound(_)
            | WorkflowEvent::AgentUpdated(_)
            | WorkflowEvent::AgentEnd(_) => None,
        };
        if let Err(error) = run.model.apply(&event) {
            tracing::warn!(run_id, %error, "ignoring invalid workflow progress event");
            return false;
        }
        // Accepted event-derived progress supersedes any previously reconciled durable fence.
        // `handle_notification` also advances the revision so a late async read cannot restore it.
        run.reconciled_status = None;
        run.status_read_pending = false;
        run.timing.observe_event(&event, observed_at);
        if let Some(log) = log {
            run.logs.push_back(log);
            while run.logs.len() > MAX_LOGS_PER_RUN {
                run.logs.pop_front();
            }
        }

        if run.model.state == WorkflowRunState::Completed {
            self.pending_status_reads.remove(run_id);
            self.prune_completed_runs();
        }
        true
    }

    fn start_run(
        &mut self,
        event: WorkflowEvent,
        observed_at: Option<i64>,
        identity: WorkflowRunIdentity,
    ) -> bool {
        let run_id = event_run_id(&event);
        if self.runs.iter().any(|run| run.model.run_id == run_id) {
            tracing::warn!(run_id, "ignoring duplicate workflow start event");
            return false;
        }

        self.prune_completed_runs();
        self.prune_completed_summarized_runs();
        let saturated_index = self
            .saturated_run_ids
            .iter()
            .position(|saturated_run_id| saturated_run_id == run_id);
        let summarized_index = self
            .summarized_runs
            .iter()
            .position(|summary| summary.run_id == run_id);
        let active_count = self
            .runs
            .iter()
            .filter(|run| run.retention_state() == WorkflowRunState::Running)
            .count();
        let model = match WorkflowRunModel::from_event(&event) {
            Ok(model) => model,
            Err(error) => {
                tracing::warn!(run_id, %error, "ignoring invalid workflow start event");
                return false;
            }
        };
        if active_count >= MAX_ACTIVE_RUNS {
            if let Some(index) = summarized_index {
                let summary = &mut self.summarized_runs[index];
                let changed =
                    summary.name != model.name || summary.status != SummarizedRunStatus::Running;
                summary.name = model.name;
                summary.status = SummarizedRunStatus::Running;
                return changed || self.remove_saturated_run(saturated_index);
            }
            let can_summarize = self.summarized_runs.len() < MAX_SUMMARIZED_RUNS
                || self
                    .summarized_runs
                    .iter()
                    .any(|summary| summary.status.is_completed());
            if saturated_index.is_some() && !can_summarize {
                return false;
            }
            self.remove_saturated_run(saturated_index);
            return self.push_summarized_run(SummarizedRun {
                run_id: model.run_id,
                name: model.name,
                status: SummarizedRunStatus::Running,
            });
        }

        if let Some(index) = summarized_index {
            if let Some(summary) = self.summarized_runs.remove(index) {
                self.forget_status_tracking(&summary.run_id);
            }
        }
        self.remove_saturated_run(saturated_index);
        let timing = WorkflowRunTiming::from_run_begin(&model, observed_at);
        self.runs.push_back(MonitoredRun {
            model,
            reconciled_status: None,
            status_read_pending: false,
            identity,
            logs: VecDeque::new(),
            save_request: WorkflowSaveRequestState::Idle,
            stop_request: WorkflowStopRequestState::Idle,
            pause_request: WorkflowPauseRequestState::Idle,
            resume_request: WorkflowResumeRequestState::Idle,
            agent_control_requests: BTreeMap::new(),
            timing,
        });
        true
    }

    fn remove_saturated_run(&mut self, index: Option<usize>) -> bool {
        let Some(index) = index else {
            return false;
        };
        let Some(run_id) = self.saturated_run_ids.remove(index) else {
            return false;
        };
        self.saturated_run_count = self.saturated_run_count.saturating_sub(1);
        self.forget_status_tracking(&run_id);
        true
    }

    fn prune_completed_runs(&mut self) {
        while self
            .runs
            .iter()
            .filter(|run| run.retention_state() == WorkflowRunState::Completed)
            .count()
            > MAX_COMPLETED_RUNS
        {
            let Some(index) = self
                .runs
                .iter()
                .position(|run| run.retention_state() == WorkflowRunState::Completed)
            else {
                break;
            };
            if let Some(run) = self.runs.remove(index) {
                self.forget_status_tracking(&run.model.run_id);
            }
        }
    }

    fn register_start_response(&mut self, run_id: String, name: String) -> bool {
        let run_id = bound_run_id(run_id);
        if self.runs.iter().any(|run| run.model.run_id == run_id)
            || self
                .saturated_run_ids
                .iter()
                .any(|saturated_run_id| saturated_run_id == &run_id)
        {
            return false;
        }
        let name = bound_text(name, MAX_SUMMARY_NAME_CHARS);
        if let Some(summary) = self
            .summarized_runs
            .iter_mut()
            .find(|summary| summary.run_id == run_id)
        {
            if summary.name == name {
                return false;
            }
            summary.name = name;
            return true;
        }
        self.push_summarized_run(SummarizedRun {
            run_id,
            name,
            status: SummarizedRunStatus::Starting,
        })
    }

    fn is_run_tracked(&self, run_id: &str) -> bool {
        self.runs.iter().any(|run| run.model.run_id == run_id)
            || self
                .summarized_runs
                .iter()
                .any(|summary| summary.run_id == run_id)
            || self
                .saturated_run_ids
                .iter()
                .any(|saturated_run_id| saturated_run_id == run_id)
    }

    fn advance_status_revision_after_event(&mut self, run_id: &str) {
        self.pending_status_reads.remove(run_id);
        if self.is_run_tracked(run_id) {
            let revision = self.status_revisions.entry(run_id.to_string()).or_default();
            *revision = revision.wrapping_add(1);
        } else {
            self.status_revisions.remove(run_id);
        }
    }

    fn forget_status_tracking(&mut self, run_id: &str) {
        self.status_revisions.remove(run_id);
        self.pending_status_reads.remove(run_id);
    }

    fn begin_status_read(&mut self, run_id: &str) -> Option<u64> {
        let can_reconcile = self
            .runs
            .iter()
            .find(|run| run.model.run_id == run_id)
            .is_some_and(|run| run.model.state == WorkflowRunState::Running)
            || self
                .summarized_runs
                .iter()
                .find(|summary| summary.run_id == run_id)
                .is_some_and(|summary| {
                    !matches!(&summary.status, SummarizedRunStatus::Completed { .. })
                })
            || self
                .saturated_run_ids
                .iter()
                .any(|saturated_run_id| saturated_run_id == run_id);
        if !can_reconcile || self.pending_status_reads.contains_key(run_id) {
            return None;
        }
        if self.pending_status_reads.len() >= MAX_PENDING_WORKFLOW_STATUS_READS {
            self.set_unknown_status_fence(run_id, /*pending*/ false);
            return None;
        }
        let revision = self.status_revisions.entry(run_id.to_string()).or_default();
        *revision = revision.wrapping_add(1);
        let revision = *revision;
        self.pending_status_reads
            .insert(run_id.to_string(), revision);
        self.set_unknown_status_fence(run_id, /*pending*/ true);
        Some(revision)
    }

    fn set_unknown_status_fence(&mut self, run_id: &str, pending: bool) {
        if let Some(run) = self.runs.iter_mut().find(|run| run.model.run_id == run_id) {
            run.status_read_pending = pending;
            run.reconciled_status = Some(WorkflowRunStatus::Unknown);
        } else if let Some(summary) = self
            .summarized_runs
            .iter_mut()
            .find(|summary| summary.run_id == run_id)
        {
            summary.status = if pending {
                SummarizedRunStatus::Reconciling
            } else {
                SummarizedRunStatus::Reconciled(WorkflowRunStatus::Unknown)
            };
        }
    }

    fn cancel_status_read(&mut self, run_id: &str, revision: u64) -> bool {
        if self.pending_status_reads.get(run_id).copied() != Some(revision) {
            return false;
        }
        self.pending_status_reads.remove(run_id);
        self.set_unknown_status_fence(run_id, /*pending*/ false);
        true
    }

    fn finish_status_read(
        &mut self,
        run_id: &str,
        revision: u64,
        status: Option<WorkflowRunStatus>,
    ) -> bool {
        if !self.cancel_status_read(run_id, revision)
            || self.status_revisions.get(run_id).copied() != Some(revision)
        {
            return false;
        }
        let Some(status) = status else {
            return false;
        };

        if let Some(run) = self.runs.iter_mut().find(|run| run.model.run_id == run_id) {
            if run.model.state == WorkflowRunState::Completed {
                return false;
            }
            let reconciled_status = match status {
                WorkflowRunStatus::Running => None,
                WorkflowRunStatus::Completed
                | WorkflowRunStatus::Stopped
                | WorkflowRunStatus::Paused
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Unknown => Some(status),
            };
            run.reconciled_status = reconciled_status;
            self.prune_completed_runs();
            self.reconcile_selection();
            return true;
        }

        if let Some(summary) = self
            .summarized_runs
            .iter_mut()
            .find(|summary| summary.run_id == run_id)
        {
            if matches!(&summary.status, SummarizedRunStatus::Completed { .. }) {
                return false;
            }
            let reconciled_status = match status {
                WorkflowRunStatus::Running => SummarizedRunStatus::Running,
                WorkflowRunStatus::Completed
                | WorkflowRunStatus::Stopped
                | WorkflowRunStatus::Paused
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Unknown => SummarizedRunStatus::Reconciled(status),
            };
            summary.status = reconciled_status;
            self.prune_completed_summarized_runs();
            return true;
        }

        if status != WorkflowRunStatus::Running {
            let saturated_index = self
                .saturated_run_ids
                .iter()
                .position(|saturated_run_id| saturated_run_id == run_id);
            return self.remove_saturated_run(saturated_index);
        }
        false
    }

    fn push_summarized_run(&mut self, summary: SummarizedRun) -> bool {
        if self.summarized_runs.len() == MAX_SUMMARIZED_RUNS
            && let Some(index) = self
                .summarized_runs
                .iter()
                .position(|summary| summary.status.is_completed())
            && let Some(summary) = self.summarized_runs.remove(index)
        {
            self.forget_status_tracking(&summary.run_id);
        }
        if self.summarized_runs.len() == MAX_SUMMARIZED_RUNS {
            self.saturated_run_count = self.saturated_run_count.saturating_add(1);
            if self.saturated_run_ids.len() == MAX_SATURATED_RUN_IDS
                && let Some(run_id) = self.saturated_run_ids.pop_front()
            {
                self.forget_status_tracking(&run_id);
            }
            self.saturated_run_ids.push_back(summary.run_id);
            return true;
        }
        self.summarized_runs.push_back(summary);
        true
    }

    fn prune_completed_summarized_runs(&mut self) {
        while self
            .summarized_runs
            .iter()
            .filter(|summary| summary.status.is_completed())
            .count()
            > MAX_COMPLETED_SUMMARIZED_RUNS
        {
            let Some(index) = self
                .summarized_runs
                .iter()
                .position(|summary| summary.status.is_completed())
            else {
                break;
            };
            if let Some(summary) = self.summarized_runs.remove(index) {
                self.forget_status_tracking(&summary.run_id);
            }
        }
    }
}

impl ChatWidget {
    pub(crate) fn on_workflow_start_succeeded(
        &mut self,
        thread_id: codex_protocol::ThreadId,
        name: String,
        run_id: String,
    ) {
        if self.thread_id != Some(thread_id) {
            return;
        }
        if self.workflow_monitor.register_start_response(run_id, name) {
            self.request_redraw();
        }
    }

    pub(super) fn on_workflow_notification(&mut self, notification: WorkflowNotification) {
        if !self.workflow_notification_targets_this_thread(notification.thread_id()) {
            return;
        }

        let status_read_target = match &notification {
            WorkflowNotification::Started(notification) => {
                ThreadId::from_string(&notification.thread_id)
                    .ok()
                    .zip(uuid::Uuid::parse_str(&notification.run_id).ok())
                    .map(|(thread_id, run_id)| (thread_id, run_id.to_string()))
            }
            WorkflowNotification::PhaseChanged(_)
            | WorkflowNotification::GroupStarted(_)
            | WorkflowNotification::GroupCompleted(_)
            | WorkflowNotification::AgentStarted(_)
            | WorkflowNotification::AgentBound(_)
            | WorkflowNotification::AgentUpdated(_)
            | WorkflowNotification::AgentCompleted(_)
            | WorkflowNotification::Log(_)
            | WorkflowNotification::Completed(_) => None,
        };
        if self.workflow_monitor.handle_notification(notification) {
            if let Some((thread_id, run_id)) = status_read_target
                && let Some(revision) = self.workflow_monitor.begin_status_read(&run_id)
            {
                self.app_event_tx.send(AppEvent::RequestWorkflowRead {
                    thread_id,
                    run_id,
                    revision,
                });
            }
            self.request_redraw();
        }
    }

    pub(crate) fn on_workflow_read_finished(
        &mut self,
        thread_id: ThreadId,
        run_id: &str,
        revision: u64,
        result: Result<WorkflowRunStatus, String>,
    ) {
        if self.thread_id != Some(thread_id) {
            self.workflow_monitor.cancel_status_read(run_id, revision);
            return;
        }
        let status = match result {
            Ok(status) => Some(status),
            Err(error) => {
                tracing::debug!(
                    %thread_id,
                    run_id,
                    error = %bounded_inline_text(&error, /*max_chars*/ 512),
                    "workflow durable status reconciliation was unavailable"
                );
                Some(WorkflowRunStatus::Unknown)
            }
        };
        if self
            .workflow_monitor
            .finish_status_read(run_id, revision, status)
        {
            self.request_redraw();
        }
    }

    pub(super) fn on_workflow_completed_notification(
        &mut self,
        notification: WorkflowCompletedNotification,
        from_replay: bool,
    ) {
        if !self.workflow_notification_targets_this_thread(&notification.thread_id) {
            return;
        }

        let context = self
            .workflow_monitor
            .runs
            .iter()
            .find(|run| run.model.run_id == notification.run_id)
            .map(|run| WorkflowCompletionContext {
                name: run.model.name.clone(),
                agent_count: Some(
                    run.model
                        .topology
                        .values()
                        .filter(|node| matches!(node, WorkflowTopologyNode::Agent(_)))
                        .count(),
                ),
            })
            .or_else(|| {
                self.workflow_monitor
                    .summarized_runs
                    .iter()
                    .find(|run| run.run_id == notification.run_id)
                    .map(|run| WorkflowCompletionContext {
                        name: run.name.clone(),
                        agent_count: None,
                    })
            });
        let completion_notification = Notification::WorkflowComplete {
            name: context
                .as_ref()
                .map(|context| context.name.clone())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| notification.run_id.clone()),
            status: notification.status.clone(),
            agent_count: context.as_ref().and_then(|context| context.agent_count),
            spent: notification.spent,
        };
        if self
            .workflow_monitor
            .handle_notification(WorkflowNotification::Completed(notification))
        {
            self.request_redraw();
        }
        if !from_replay {
            self.notify(completion_notification);
        }
    }

    fn workflow_notification_targets_this_thread(&self, actual_thread_id: &str) -> bool {
        let expected_thread_id = self.thread_id().map(|thread_id| thread_id.to_string());
        if let Some(expected) = expected_thread_id.as_deref()
            && expected != actual_thread_id
        {
            tracing::warn!(
                expected,
                actual = actual_thread_id,
                "ignoring misrouted workflow notification"
            );
            return false;
        }
        true
    }
}

fn event_run_id(event: &WorkflowEvent) -> &str {
    match event {
        WorkflowEvent::RunBegin(event) => &event.run_id,
        WorkflowEvent::RunEnd(event) => &event.run_id,
        WorkflowEvent::PhaseBegin(event) => &event.run_id,
        WorkflowEvent::PhaseEnd(event) => &event.run_id,
        WorkflowEvent::GroupBegin(event) => &event.run_id,
        WorkflowEvent::GroupEnd(event) => &event.run_id,
        WorkflowEvent::AgentBegin(event) => &event.run_id,
        WorkflowEvent::AgentBound(event) => &event.run_id,
        WorkflowEvent::AgentUpdated(event) => &event.run_id,
        WorkflowEvent::AgentEnd(event) => &event.run_id,
        WorkflowEvent::Log(event) => &event.run_id,
    }
}

fn bounded_log(message: &str) -> Option<String> {
    let normalized = bounded_inline_text(message, MAX_LOG_CHARS);
    (!normalized.is_empty()).then_some(normalized)
}

fn bound_text(text: String, max_chars: usize) -> String {
    bounded_inline_text(&text, max_chars)
}

fn bound_run_id(run_id: String) -> String {
    bound_text(run_id, MAX_RUN_ID_CHARS)
}

fn durable_name_is_display_safe(name: &str) -> bool {
    name.len() <= MAX_DURABLE_NAME_BYTES
        && name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn bounded_inline_text(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut normalized = String::new();
    let mut char_count = 0;
    let mut truncated = false;
    'words: for word in text.split_whitespace() {
        if !normalized.is_empty() {
            if char_count == max_chars {
                truncated = true;
                break;
            }
            normalized.push(' ');
            char_count += 1;
        }
        for ch in word.chars() {
            if char_count == max_chars {
                truncated = true;
                break 'words;
            }
            normalized.push(ch);
            char_count += 1;
        }
    }
    if truncated {
        normalized.pop();
        normalized.push('…');
    }
    normalized
}

#[cfg(test)]
#[path = "workflow_monitor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "workflow_control_tests.rs"]
mod workflow_control_tests;
