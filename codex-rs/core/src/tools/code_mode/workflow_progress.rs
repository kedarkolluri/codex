//! Core boundary for durable workflow progress events.

pub(crate) mod durable;

use codex_code_mode::CellId;
use codex_code_mode::WorkflowHostCompletion;
use codex_code_mode::WorkflowHostProgress;
use codex_core_workflows::WorkflowBudget;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentBoundEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus as JournalRunStatus;
use codex_workflow_journal::storage::WorkflowRunPaths;
use std::sync::Arc;
use tracing::warn;

use super::ExecContext;
use super::delegate::CodeModeDispatchOrigin;
use super::workflow_handler::WorkflowRunLedger;

const MAX_PROGRESS_TEXT_BYTES: usize = 256;
const MAX_PROGRESS_ERROR_BYTES: usize = 2_048;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkflowRunCompletion {
    Completed,
    Errored(String),
    Interrupted,
    Stopped,
    Paused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkflowTerminalPublication {
    pub(crate) progress_persisted: bool,
    pub(crate) metadata_persisted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkflowRunIndexAdmission {
    Unindexed,
    InsertedPending,
    ExistingPending(codex_state::WorkflowRunStatus),
    ExistingCommitted(codex_state::WorkflowRunStatus),
}

impl WorkflowRunCompletion {
    pub(crate) fn journal_status(&self) -> JournalRunStatus {
        match self {
            Self::Completed => JournalRunStatus::Completed,
            Self::Stopped => JournalRunStatus::Stopped,
            Self::Paused => JournalRunStatus::Paused,
            Self::Errored(_) | Self::Interrupted => JournalRunStatus::Failed,
        }
    }
}

impl From<WorkflowHostCompletion> for WorkflowRunCompletion {
    fn from(completion: WorkflowHostCompletion) -> Self {
        match completion {
            WorkflowHostCompletion::Completed => Self::Completed,
            WorkflowHostCompletion::Errored(error) => Self::Errored(error),
            WorkflowHostCompletion::Interrupted => Self::Interrupted,
        }
    }
}

#[derive(Clone)]
pub(crate) enum WorkflowEventTarget {
    Session {
        exec: ExecContext,
        dispatch_origin: CodeModeDispatchOrigin,
    },
    #[cfg(test)]
    Disabled,
}

impl WorkflowEventTarget {
    pub(crate) fn session(exec: ExecContext) -> Self {
        let dispatch_origin = CodeModeDispatchOrigin::Turn(exec.turn.sub_id.clone());
        Self::Session {
            exec,
            dispatch_origin,
        }
    }

    pub(crate) fn nested(exec: ExecContext, parent_cell_id: CellId) -> Self {
        Self::Session {
            exec,
            dispatch_origin: CodeModeDispatchOrigin::ParentCell(parent_cell_id),
        }
    }

    pub(crate) fn dispatch_origin(&self) -> CodeModeDispatchOrigin {
        match self {
            Self::Session {
                dispatch_origin, ..
            } => dispatch_origin.clone(),
            #[cfg(test)]
            Self::Disabled => CodeModeDispatchOrigin::Disabled,
        }
    }

    pub(crate) fn owner_thread_id(&self) -> Option<String> {
        match self {
            Self::Session { exec, .. } => Some(exec.session.thread_id().to_string()),
            #[cfg(test)]
            Self::Disabled => None,
        }
    }

    pub(crate) fn parent_budget(&self, ledger: &WorkflowRunLedger) -> Option<Arc<WorkflowBudget>> {
        match self {
            Self::Session {
                dispatch_origin: CodeModeDispatchOrigin::ParentCell(cell_id),
                ..
            } => ledger.budget_for_cell(cell_id),
            Self::Session {
                dispatch_origin: CodeModeDispatchOrigin::Turn(_),
                ..
            } => None,
            #[cfg(test)]
            Self::Session {
                dispatch_origin: CodeModeDispatchOrigin::Disabled,
                ..
            }
            | Self::Disabled => None,
        }
    }

    /// Opaque, non-secret fingerprint of the effective provider/router/model
    /// environment used by inherited workflow agents.
    pub(crate) async fn execution_fingerprint(&self) -> Option<String> {
        match self {
            Self::Session { exec, .. } => {
                Some(super::workflow_replay_fingerprint::for_exec(exec).await)
            }
            #[cfg(test)]
            Self::Disabled => None,
        }
    }

    pub(crate) async fn emit(&self, event: WorkflowEvent) {
        match self {
            Self::Session { exec, .. } => emit_unregistered(exec, event).await,
            #[cfg(test)]
            Self::Disabled => {}
        }
    }

    /// Reserve the discovery projection before any run artifact is published.
    pub(crate) async fn begin_run_publication(
        &self,
        paths: &WorkflowRunPaths,
        meta: &WorkflowRunMeta,
    ) -> anyhow::Result<WorkflowRunIndexAdmission> {
        let exec = match self {
            Self::Session { exec, .. } => exec,
            #[cfg(test)]
            Self::Disabled => return Ok(WorkflowRunIndexAdmission::Unindexed),
        };
        let Some(state) = exec.session.services.state_db.as_ref() else {
            return Ok(WorkflowRunIndexAdmission::Unindexed);
        };
        let params = codex_state::WorkflowRunUpsertParams {
            run_id: meta.run_id.clone(),
            name: meta.name.clone(),
            script_hash: meta.script_hash.clone(),
            script_path: paths.script().display().to_string(),
            parent_run_id: meta.parent_run_id.clone(),
            resumed_from_run_id: meta.resumed_from_run_id.clone(),
            owner_thread_id: meta.owner_thread_id.clone(),
            status: codex_state::WorkflowRunStatus::Running,
            created_at: meta.created_at.clone(),
        };
        Ok(match state.begin_workflow_run_publication(&params).await? {
            codex_state::WorkflowRunPublicationAdmission::InsertedPending => {
                WorkflowRunIndexAdmission::InsertedPending
            }
            codex_state::WorkflowRunPublicationAdmission::ExistingPending(status) => {
                WorkflowRunIndexAdmission::ExistingPending(status)
            }
            codex_state::WorkflowRunPublicationAdmission::ExistingCommitted(status) => {
                WorkflowRunIndexAdmission::ExistingCommitted(status)
            }
        })
    }

    /// Commit a reserved discovery row after authenticated artifacts exist.
    pub(crate) async fn commit_run_publication(
        &self,
        run_id: &str,
        journal_status: JournalRunStatus,
    ) -> anyhow::Result<()> {
        let exec = match self {
            Self::Session { exec, .. } => exec,
            #[cfg(test)]
            Self::Disabled => return Ok(()),
        };
        let Some(state) = exec.session.services.state_db.as_ref() else {
            return Ok(());
        };
        if !state.commit_workflow_run_publication(run_id).await? {
            anyhow::bail!("workflow run `{run_id}` has no pending publication reservation");
        }
        if journal_status != JournalRunStatus::Running {
            let status = match journal_status {
                JournalRunStatus::Running => codex_state::WorkflowRunStatus::Running,
                JournalRunStatus::Completed => codex_state::WorkflowRunStatus::Completed,
                JournalRunStatus::Stopped => codex_state::WorkflowRunStatus::Stopped,
                JournalRunStatus::Paused => codex_state::WorkflowRunStatus::Paused,
                JournalRunStatus::Failed => codex_state::WorkflowRunStatus::Failed,
            };
            if !state.set_workflow_run_status(run_id, status).await? {
                anyhow::bail!("workflow run `{run_id}` publication status could not be committed");
            }
        }
        Ok(())
    }

    /// Remove a reservation after publication fails before commit.
    pub(crate) async fn abort_run_publication(&self, run_id: &str) {
        let exec = match self {
            Self::Session { exec, .. } => exec,
            #[cfg(test)]
            Self::Disabled => return,
        };
        if let Some(state) = exec.session.services.state_db.as_ref()
            && let Err(error) = state.abort_workflow_run_publication(run_id).await
        {
            warn!("failed to abort workflow publication row for {run_id}: {error}");
        }
    }

    /// Persist a terminal status to both `meta.json` and the optional SQLite
    /// discovery projection. The on-disk metadata remains authoritative.
    pub(crate) async fn record_run_finished(
        &self,
        run_id: &str,
        journal_status: JournalRunStatus,
    ) -> bool {
        let exec = match self {
            Self::Session { exec, .. } => exec,
            #[cfg(test)]
            Self::Disabled => return false,
        };
        let paths = WorkflowRunPaths::new(exec.turn.config.codex_home.as_path(), run_id);
        let authoritative_status = match paths.update_status(journal_status) {
            Ok(meta) => meta.status,
            Err(error) => {
                warn!("failed to persist terminal workflow status for {run_id}: {error}");
                return false;
            }
        };
        let index_status = match authoritative_status {
            JournalRunStatus::Running => codex_state::WorkflowRunStatus::Running,
            JournalRunStatus::Completed => codex_state::WorkflowRunStatus::Completed,
            JournalRunStatus::Stopped => codex_state::WorkflowRunStatus::Stopped,
            JournalRunStatus::Paused => codex_state::WorkflowRunStatus::Paused,
            JournalRunStatus::Failed => codex_state::WorkflowRunStatus::Failed,
        };
        if let Some(state) = exec.session.services.state_db.as_ref()
            && let Err(error) = state.set_workflow_run_status(run_id, index_status).await
        {
            warn!("failed to update workflow_runs status for {run_id}: {error}");
        }
        authoritative_status == journal_status
    }

    pub(crate) async fn complete_cell(
        &self,
        ledger: &WorkflowRunLedger,
        cell_id: &CellId,
        completion: WorkflowRunCompletion,
    ) -> Option<WorkflowTerminalPublication> {
        let Some(facts) = ledger.claim_terminal(cell_id) else {
            return None;
        };
        Some(self.complete_claimed_cell(facts, completion).await)
    }

    /// Reserve the terminal ledger claim without publishing it yet.
    ///
    /// Pause uses this before runtime termination, then publishes only after
    /// all child callbacks have drained.
    pub(in crate::tools::code_mode) fn claim_cell_terminal(
        &self,
        ledger: &WorkflowRunLedger,
        cell_id: &CellId,
    ) -> Option<super::workflow_handler::WorkflowRunTerminalFacts> {
        ledger.claim_terminal(cell_id)
    }

    /// Publish a previously reserved terminal claim.
    pub(in crate::tools::code_mode) async fn complete_claimed_cell(
        &self,
        facts: super::workflow_handler::WorkflowRunTerminalFacts,
        completion: WorkflowRunCompletion,
    ) -> WorkflowTerminalPublication {
        match self {
            Self::Session { exec, .. } => emit_claimed_terminal(exec, facts, completion).await,
            #[cfg(test)]
            Self::Disabled => WorkflowTerminalPublication {
                progress_persisted: true,
                metadata_persisted: false,
            },
        }
    }
}

pub(super) async fn handle_host_progress(
    exec: &ExecContext,
    ledger: &WorkflowRunLedger,
    cell_id: &CellId,
    progress: WorkflowHostProgress,
) {
    match progress {
        WorkflowHostProgress::Event { event } => {
            emit_for_cell(exec, ledger, cell_id, *event).await;
        }
        WorkflowHostProgress::Complete { status } => {
            emit_terminal(exec, ledger, cell_id, status.into()).await;
        }
    }
}

pub(super) struct WorkflowAgentBeginParams<'a> {
    pub(super) node_id: u64,
    pub(super) attempt: u32,
    pub(super) last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub(super) parent_node_id: Option<u64>,
    pub(super) requested_label: Option<&'a str>,
    pub(super) ordinal: u64,
    pub(super) phase: Option<String>,
    pub(super) model: String,
    pub(super) effort: ReasoningEffort,
}

pub(super) struct WorkflowAgentUpdateParams {
    pub(super) node_id: u64,
    pub(super) attempt: u32,
    pub(super) last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub(super) token_usage: TokenUsage,
    pub(super) tool_call_count: u64,
    pub(super) duration_ms: u64,
}

pub(super) async fn emit_agent_begin(
    exec: &ExecContext,
    ledger: &WorkflowRunLedger,
    cell_id: &CellId,
    params: WorkflowAgentBeginParams<'_>,
) -> bool {
    let Some(run_id) = ledger.parent_run_id_for_cell(cell_id) else {
        return false;
    };
    let fallback = format!("agent {}", params.ordinal.saturating_add(1));
    let label = params
        .requested_label
        .filter(|label| !label.trim().is_empty())
        .map(|label| bound_text(label, MAX_PROGRESS_TEXT_BYTES))
        .unwrap_or(fallback);
    emit_for_cell(
        exec,
        ledger,
        cell_id,
        WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id,
            node_id: params.node_id,
            attempt: params.attempt,
            last_attempt_reason: params.last_attempt_reason,
            parent_node_id: params.parent_node_id,
            label,
            phase: params.phase,
            model: params.model,
            effort: params.effort,
        }),
    )
    .await
}

pub(super) async fn emit_agent_update(
    exec: &ExecContext,
    ledger: &WorkflowRunLedger,
    cell_id: &CellId,
    params: WorkflowAgentUpdateParams,
) {
    let Some(run_id) = ledger.parent_run_id_for_cell(cell_id) else {
        return;
    };
    emit_for_cell(
        exec,
        ledger,
        cell_id,
        WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id,
            node_id: params.node_id,
            attempt: params.attempt,
            last_attempt_reason: params.last_attempt_reason,
            token_usage: params.token_usage,
            tool_call_count: params.tool_call_count,
            duration_ms: params.duration_ms,
        }),
    )
    .await;
}

pub(super) async fn emit_agent_bound(
    exec: &ExecContext,
    ledger: &WorkflowRunLedger,
    cell_id: &CellId,
    node_id: u64,
    attempt: u32,
    child_thread_id: ThreadId,
) -> bool {
    let Some(run_id) = ledger.parent_run_id_for_cell(cell_id) else {
        return false;
    };
    emit_for_cell(
        exec,
        ledger,
        cell_id,
        WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
            run_id,
            node_id,
            attempt,
            child_thread_id: child_thread_id.to_string(),
        }),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn emit_agent_end(
    exec: &ExecContext,
    ledger: &WorkflowRunLedger,
    cell_id: &CellId,
    node_id: u64,
    attempt: u32,
    last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    status: AgentStatus,
    token_usage: TokenUsage,
    tool_call_count: u64,
    duration_ms: u64,
    returned_null: bool,
) {
    let Some(run_id) = ledger.parent_run_id_for_cell(cell_id) else {
        return;
    };
    emit_for_cell(
        exec,
        ledger,
        cell_id,
        WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
            run_id,
            node_id,
            attempt,
            last_attempt_reason,
            status,
            token_usage,
            tool_call_count,
            duration_ms,
            returned_null,
        }),
    )
    .await;
}

pub(super) async fn emit_unregistered(exec: &ExecContext, event: WorkflowEvent) {
    if let Err(error) = durable::record_event(exec.turn.config.codex_home.as_path(), &event).await {
        warn!("failed to persist workflow progress projection: {error}");
    }
    send_unregistered(exec, event).await;
}

async fn send_unregistered(exec: &ExecContext, event: WorkflowEvent) {
    exec.session
        .send_event(&exec.turn, EventMsg::Workflow(event))
        .await;
}

async fn emit_for_cell(
    exec: &ExecContext,
    ledger: &WorkflowRunLedger,
    cell_id: &CellId,
    event: WorkflowEvent,
) -> bool {
    if !ledger.observe_progress(cell_id, &event) {
        warn!("dropping workflow progress for unregistered or mismatched cell {cell_id}");
        return false;
    }
    emit_unregistered(exec, event).await;
    true
}

async fn emit_terminal(
    exec: &ExecContext,
    ledger: &WorkflowRunLedger,
    cell_id: &CellId,
    completion: WorkflowRunCompletion,
) -> bool {
    let Some(facts) = ledger.claim_terminal(cell_id) else {
        return false;
    };
    let _ = emit_claimed_terminal(exec, facts, completion).await;
    true
}

async fn emit_claimed_terminal(
    exec: &ExecContext,
    mut facts: super::workflow_handler::WorkflowRunTerminalFacts,
    completion: WorkflowRunCompletion,
) -> WorkflowTerminalPublication {
    let paused = matches!(&completion, WorkflowRunCompletion::Paused);
    let journal_status = completion.journal_status();
    let (status, terminal_reason) = completion_status(completion);

    // Cancellation can terminate V8 before a pending orchestration promise runs its `.finally`.
    // Close any surviving leaves, then groups from the inside out, then the explicit phase. Normal
    // completion leaves all three collections empty, so this is a no-op on the hot path.
    facts.active_agents.sort_by_key(|agent| agent.node_id);
    for agent in facts.active_agents.into_iter().rev() {
        emit_unregistered(
            exec,
            WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
                run_id: facts.run_id.clone(),
                node_id: agent.node_id,
                attempt: agent.attempt,
                last_attempt_reason: agent.last_attempt_reason,
                status: AgentStatus::Interrupted,
                token_usage: agent.token_usage,
                tool_call_count: agent.tool_call_count,
                duration_ms: agent.duration_ms,
                returned_null: true,
            }),
        )
        .await;
    }
    for group in facts.active_groups.into_iter().rev() {
        emit_unregistered(
            exec,
            WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
                run_id: facts.run_id.clone(),
                group_id: group.group_id,
                kind: group.kind,
                item_count: group.item_count,
            }),
        )
        .await;
    }
    if let Some(phase) = facts.active_phase {
        emit_unregistered(
            exec,
            WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
                run_id: facts.run_id.clone(),
                phase_index: phase.phase_index,
                title: phase.title,
            }),
        )
        .await;
    }
    let spent = i64::try_from(facts.budget.snapshot().spent).unwrap_or(i64::MAX);
    let run_id = facts.run_id;
    let terminal_event = WorkflowEvent::RunEnd(WorkflowRunEndEvent {
        run_id: run_id.clone(),
        status,
        terminal_reason: Some(terminal_reason),
        spent,
        total: facts.budget_total.map(|total| total.max(0)),
    });
    // Publish the atomic terminal snapshot before flipping meta.json. A detached watcher can then
    // trust terminal metadata without racing the last progress write. Projection failure remains
    // best-effort: metadata and the live transport still reach their terminal state.
    let progress_result = if paused {
        durable::record_terminal_event(
            exec.turn.config.codex_home.as_path(),
            &terminal_event,
            durable::DurableRunStatus::Paused,
        )
        .await
    } else {
        durable::record_event(exec.turn.config.codex_home.as_path(), &terminal_event).await
    };
    let progress_persisted = progress_result.is_ok();
    if let Err(error) = progress_result {
        warn!("failed to persist terminal workflow progress projection: {error}");
    }
    let journal_status = if paused && !progress_persisted {
        JournalRunStatus::Failed
    } else {
        journal_status
    };
    let metadata_persisted = WorkflowEventTarget::session(exec.clone())
        .record_run_finished(&run_id, journal_status)
        .await;
    send_unregistered(exec, terminal_event).await;
    WorkflowTerminalPublication {
        progress_persisted,
        metadata_persisted,
    }
}

fn completion_status(
    completion: WorkflowRunCompletion,
) -> (AgentStatus, WorkflowRunTerminalReason) {
    match completion {
        WorkflowRunCompletion::Completed => (
            AgentStatus::Completed(None),
            WorkflowRunTerminalReason::Completed,
        ),
        WorkflowRunCompletion::Errored(error) => (
            AgentStatus::Errored(bound_text(&error, MAX_PROGRESS_ERROR_BYTES)),
            WorkflowRunTerminalReason::Failed,
        ),
        WorkflowRunCompletion::Interrupted => (
            AgentStatus::Interrupted,
            WorkflowRunTerminalReason::Interrupted,
        ),
        WorkflowRunCompletion::Paused => {
            (AgentStatus::Interrupted, WorkflowRunTerminalReason::Paused)
        }
        // `Shutdown` is the established shared wire value. At run scope it
        // means an authenticated explicit stop, and the durable reducer maps it
        // to its own `stopped` status.
        WorkflowRunCompletion::Stopped => {
            (AgentStatus::Shutdown, WorkflowRunTerminalReason::Stopped)
        }
    }
}

fn bound_text(text: &str, max_bytes: usize) -> String {
    const MARKER: &str = "… [truncated]";
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes.saturating_sub(MARKER.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &text[..end])
}
