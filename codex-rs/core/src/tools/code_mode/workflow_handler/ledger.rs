use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode::CellId;
use codex_core_workflows::WorkflowBudget;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowAgentBoundEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_workflow_journal::JournalRecorder;

/// Where a workflow run sits in the nesting tree, threaded into workflow start
/// so the ledger records both facts when the run's isolate cell is created.
pub(crate) struct WorkflowRunLineage {
    /// Run id of the workflow whose isolate spawned this one, or `None` for a
    /// top-level (model-callable) run.
    pub(crate) parent_run_id: Option<String>,
    /// `workflow()` nesting depth: 0 for a top-level model-callable run, 1 for a
    /// run spawned by a depth-0 workflow, …
    pub(crate) depth: i32,
}

/// One recorded workflow run → parent linkage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkflowRunLink {
    /// Host-minted run id of this run (`workflow.runId`).
    pub(crate) run_id: String,
    /// Run id of the workflow whose isolate spawned this one, or `None` for a
    /// top-level (model-callable) workflow run.
    pub(crate) parent_run_id: Option<String>,
}

/// One workflow run's isolate-cell bookkeeping.
#[derive(Clone)]
struct CellRun {
    run_id: String,
    depth: i32,
    budget: Arc<WorkflowBudget>,
    budget_total: Option<i64>,
    terminal_emitted: bool,
    active_phase: Option<WorkflowPhaseBeginEvent>,
    active_groups: Vec<WorkflowGroupBeginEvent>,
    active_agents: HashMap<u64, ActiveAgentProgress>,
}

#[derive(Clone, Debug, Default)]
struct ActiveAgentProgress {
    attempt: u32,
    last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    token_usage: TokenUsage,
    tool_call_count: u64,
    duration_ms: u64,
}

pub(in crate::tools::code_mode) struct ActiveAgentTerminalProgress {
    pub(in crate::tools::code_mode) node_id: u64,
    pub(in crate::tools::code_mode) attempt: u32,
    pub(in crate::tools::code_mode) last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub(in crate::tools::code_mode) token_usage: TokenUsage,
    pub(in crate::tools::code_mode) tool_call_count: u64,
    pub(in crate::tools::code_mode) duration_ms: u64,
}

pub(in crate::tools::code_mode) struct WorkflowRunTerminalFacts {
    pub(in crate::tools::code_mode) run_id: String,
    pub(in crate::tools::code_mode) budget: Arc<WorkflowBudget>,
    pub(in crate::tools::code_mode) budget_total: Option<i64>,
    pub(in crate::tools::code_mode) active_phase: Option<WorkflowPhaseBeginEvent>,
    pub(in crate::tools::code_mode) active_groups: Vec<WorkflowGroupBeginEvent>,
    pub(in crate::tools::code_mode) active_agents: Vec<ActiveAgentTerminalProgress>,
}

/// In-memory ledger of workflow runs keyed by the isolate cell executing them.
#[derive(Default)]
pub(crate) struct WorkflowRunLedger {
    cell_runs: Mutex<HashMap<CellId, CellRun>>,
    links: Mutex<Vec<WorkflowRunLink>>,
    /// Per-run recorder keyed by the isolate cell executing the run.
    recorders: Mutex<HashMap<CellId, Arc<JournalRecorder>>>,
}

impl WorkflowRunLedger {
    /// Record a freshly-minted run executing in `cell_id` at nesting `depth`,
    /// linked to `parent_run_id`.
    pub(super) fn register_run(
        &self,
        cell_id: CellId,
        run_id: String,
        parent_run_id: Option<String>,
        depth: i32,
        budget: Arc<WorkflowBudget>,
        budget_total: Option<i64>,
    ) {
        if let Ok(mut cell_runs) = self.cell_runs.lock() {
            cell_runs.insert(
                cell_id,
                CellRun {
                    run_id: run_id.clone(),
                    depth,
                    budget,
                    budget_total,
                    terminal_emitted: false,
                    active_phase: None,
                    active_groups: Vec::new(),
                    active_agents: HashMap::new(),
                },
            );
        }
        if let Ok(mut links) = self.links.lock() {
            links.push(WorkflowRunLink {
                run_id,
                parent_run_id,
            });
        }
    }

    /// The run id of the workflow executing in `cell_id`, if any.
    pub(crate) fn parent_run_id_for_cell(&self, cell_id: &CellId) -> Option<String> {
        self.cell_runs
            .lock()
            .ok()
            .and_then(|cell_runs| cell_runs.get(cell_id).map(|run| run.run_id.clone()))
    }

    /// The `workflow()` nesting depth of the run executing in `cell_id`, if known.
    pub(crate) fn depth_for_cell(&self, cell_id: &CellId) -> Option<i32> {
        self.cell_runs
            .lock()
            .ok()
            .and_then(|cell_runs| cell_runs.get(cell_id).map(|run| run.depth))
    }

    pub(crate) fn budget_for_cell(&self, cell_id: &CellId) -> Option<Arc<WorkflowBudget>> {
        self.cell_runs
            .lock()
            .ok()
            .and_then(|cell_runs| cell_runs.get(cell_id).map(|run| Arc::clone(&run.budget)))
    }

    /// Atomically claim the one public terminal event for `cell_id` and return its budget baseline.
    pub(in crate::tools::code_mode) fn claim_terminal(
        &self,
        cell_id: &CellId,
    ) -> Option<WorkflowRunTerminalFacts> {
        let mut cell_runs = self.cell_runs.lock().ok()?;
        let run = cell_runs.get_mut(cell_id)?;
        if run.terminal_emitted {
            return None;
        }
        run.terminal_emitted = true;
        Some(WorkflowRunTerminalFacts {
            run_id: run.run_id.clone(),
            budget: Arc::clone(&run.budget),
            budget_total: run.budget_total,
            active_phase: run.active_phase.take(),
            active_groups: std::mem::take(&mut run.active_groups),
            active_agents: std::mem::take(&mut run.active_agents)
                .into_iter()
                .map(|(node_id, progress)| ActiveAgentTerminalProgress {
                    node_id,
                    attempt: progress.attempt,
                    last_attempt_reason: progress.last_attempt_reason,
                    token_usage: progress.token_usage,
                    tool_call_count: progress.tool_call_count,
                    duration_ms: progress.duration_ms,
                })
                .collect(),
        })
    }

    /// Track enough reducer state to close topology deterministically after cancellation or failure.
    pub(in crate::tools::code_mode) fn observe_progress(
        &self,
        cell_id: &CellId,
        event: &WorkflowEvent,
    ) -> bool {
        let Ok(mut cell_runs) = self.cell_runs.lock() else {
            return false;
        };
        let Some(run) = cell_runs.get_mut(cell_id) else {
            return false;
        };
        if run.terminal_emitted {
            return false;
        }
        if workflow_event_run_id(event) != run.run_id {
            return false;
        }
        match event {
            WorkflowEvent::PhaseBegin(event) => run.active_phase = Some(event.clone()),
            WorkflowEvent::PhaseEnd(event) => {
                if run
                    .active_phase
                    .as_ref()
                    .is_some_and(|phase| phase.phase_index == event.phase_index)
                {
                    run.active_phase = None;
                }
            }
            WorkflowEvent::GroupBegin(event) => run.active_groups.push(event.clone()),
            WorkflowEvent::GroupEnd(event) => {
                run.active_groups
                    .retain(|group| group.group_id != event.group_id);
            }
            WorkflowEvent::AgentBegin(event) => match run.active_agents.entry(event.node_id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    if event.attempt != 0 || event.last_attempt_reason.is_some() {
                        return false;
                    }
                    entry.insert(ActiveAgentProgress::default());
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let progress = entry.get_mut();
                    if event.attempt == progress.attempt
                        && event.last_attempt_reason == progress.last_attempt_reason
                    {
                        return true;
                    }
                    if event.attempt != progress.attempt.saturating_add(1)
                        || event.last_attempt_reason != Some(WorkflowAgentAttemptReason::UserRetry)
                    {
                        return false;
                    }
                    progress.attempt = event.attempt;
                    progress.last_attempt_reason = event.last_attempt_reason;
                }
            },
            WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
                node_id, attempt, ..
            }) => {
                if run
                    .active_agents
                    .get(node_id)
                    .is_none_or(|progress| progress.attempt != *attempt)
                {
                    return false;
                }
            }
            WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
                node_id,
                attempt,
                last_attempt_reason,
                token_usage,
                tool_call_count,
                duration_ms,
                ..
            }) => {
                if let Some(progress) = run.active_agents.get_mut(node_id) {
                    if progress.attempt != *attempt
                        || progress.last_attempt_reason != *last_attempt_reason
                        || counters_regress(progress, token_usage, *tool_call_count, *duration_ms)
                    {
                        return false;
                    }
                    progress.token_usage.clone_from(token_usage);
                    progress.tool_call_count = *tool_call_count;
                    progress.duration_ms = *duration_ms;
                } else {
                    return false;
                }
            }
            WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
                node_id, attempt, ..
            }) => {
                if run
                    .active_agents
                    .get(node_id)
                    .is_none_or(|progress| progress.attempt != *attempt)
                {
                    return false;
                }
                run.active_agents.remove(node_id);
            }
            WorkflowEvent::RunBegin(_) | WorkflowEvent::RunEnd(_) | WorkflowEvent::Log(_) => {}
        }
        true
    }

    /// Register the run's recorder under the cell executing it.
    pub(crate) fn register_recorder(&self, cell_id: CellId, recorder: Arc<JournalRecorder>) {
        if let Ok(mut recorders) = self.recorders.lock() {
            recorders.insert(cell_id, recorder);
        }
    }

    /// The recorder for the run executing in `cell_id`, if this is a journaled workflow run.
    pub(crate) fn recorder_for_cell(&self, cell_id: &CellId) -> Option<Arc<JournalRecorder>> {
        self.recorders
            .lock()
            .ok()
            .and_then(|recorders| recorders.get(cell_id).cloned())
    }

    /// Drop transient cell state once a cell reaches a terminal state.
    pub(crate) fn forget_cell(&self, cell_id: &CellId) {
        if let Ok(mut cell_runs) = self.cell_runs.lock() {
            cell_runs.remove(cell_id);
        }
        if let Ok(mut recorders) = self.recorders.lock() {
            recorders.remove(cell_id);
        }
    }

    /// Snapshot every recorded run→parent link in registration order.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn links(&self) -> Vec<WorkflowRunLink> {
        self.links
            .lock()
            .map(|links| links.clone())
            .unwrap_or_default()
    }
}

fn counters_regress(
    previous: &ActiveAgentProgress,
    token_usage: &TokenUsage,
    tool_call_count: u64,
    duration_ms: u64,
) -> bool {
    token_usage.input_tokens < previous.token_usage.input_tokens
        || token_usage.cached_input_tokens < previous.token_usage.cached_input_tokens
        || token_usage.output_tokens < previous.token_usage.output_tokens
        || token_usage.reasoning_output_tokens < previous.token_usage.reasoning_output_tokens
        || token_usage.total_tokens < previous.token_usage.total_tokens
        || tool_call_count < previous.tool_call_count
        || duration_ms < previous.duration_ms
}

fn workflow_event_run_id(event: &WorkflowEvent) -> &str {
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

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
