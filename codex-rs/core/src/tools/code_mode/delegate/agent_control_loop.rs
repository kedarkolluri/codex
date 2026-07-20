use std::time::Instant;

use super::*;
use crate::tools::code_mode::workflow_agent_controls::AttemptFinish;

const CONTROL_REGISTRATION_ERROR: &str = "workflow agent attempt is unavailable";

impl CoreTurnHost {
    /// Drive one logical workflow `agent()` call across its initial attempt and any selected
    /// user-requested retries. The invocation ordinal and topology node remain fixed; every retry
    /// re-enters the ordinary scheduler/spawn/worktree/budget path with a fresh child thread.
    pub(super) async fn spawn_agent(
        &self,
        invocation: WorkflowAgentInvocation,
        progress_context: WorkflowAgentProgressContext,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnOutcome {
        // Validate before hashing or retaining any workflow-authored fragment. Process-owned hosts
        // cross the same core boundary, so this gate protects both host modes.
        if let Err(reason) = validate_agent_invocation(&invocation) {
            warn!("workflow agent() rejected: {reason}");
            return AgentSpawnOutcome::Rejected(reason);
        }

        let service = &self.exec.session.services.code_mode_service;
        let ledger = service.workflow_run_ledger();
        let Some(run_id) = ledger.parent_run_id_for_cell(&invocation.cell_id) else {
            warn!("{CONTROL_REGISTRATION_ERROR}");
            return AgentSpawnOutcome::Rejected(CONTROL_REGISTRATION_ERROR.to_string());
        };
        let journal = AgentCallJournalCtx::for_invocation(
            ledger.recorder_for_cell(&invocation.cell_id),
            &invocation,
        );
        let controls = Arc::clone(service.workflow_agent_controls());
        let mut attempt = 0;
        let mut aggregate_progress = WorkflowChildProgress::default();
        let mut aggregate_duration_ms = 0_u64;
        let mut aggregate_tokens_spent = 0_u64;
        let mut had_metered_attempt = false;
        let mut ever_bound = false;

        loop {
            if cancellation_token.is_cancelled() {
                if let Err(reason) = journal
                    .record(AgentCallRecord {
                        attempt,
                        status: None,
                        control_reason: None,
                        ret: JsonValue::Null,
                        child_thread_id: None,
                        rollout_path: None,
                        tokens_spent: had_metered_attempt.then_some(aggregate_tokens_spent),
                        progress: Some(journal_progress(
                            &aggregate_progress,
                            aggregate_duration_ms,
                        )),
                    })
                    .await
                {
                    return AgentSpawnOutcome::Rejected(reason);
                }
                return AgentSpawnOutcome::Failed;
            }

            let attempt_cancellation = cancellation_token.child_token();
            let Some(registration) = controls.register_attempt(
                &run_id,
                progress_context.node_id,
                attempt,
                attempt_cancellation.clone(),
            ) else {
                warn!("{CONTROL_REGISTRATION_ERROR}");
                if let Err(reason) = journal.record_error(attempt).await {
                    return AgentSpawnOutcome::Rejected(reason);
                }
                return AgentSpawnOutcome::Rejected(CONTROL_REGISTRATION_ERROR.to_string());
            };

            let started_at = Instant::now();
            let execution = self.spawn_agent_attempt(
                invocation.clone(),
                progress_context.clone(),
                WorkflowAgentAttemptContext {
                    attempt,
                    cancellation_token: attempt_cancellation,
                    journal: Arc::clone(&journal),
                    prior_progress: aggregate_progress.clone(),
                    prior_duration_ms: aggregate_duration_ms,
                    started_at,
                    activation: registration.activation(),
                },
            );
            tokio::pin!(execution);
            let result = tokio::select! {
                result = &mut execution => result,
                _ = cancellation_token.cancelled() => {
                    registration.claim_run_cancellation();
                    execution.await
                }
            };
            let attempt_duration_ms =
                u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            aggregate_duration_ms = aggregate_duration_ms.saturating_add(attempt_duration_ms);

            aggregate_progress = add_workflow_child_progress(&aggregate_progress, &result.progress);
            if let Some(tokens_spent) = result.execution.tokens_spent {
                had_metered_attempt = true;
                aggregate_tokens_spent = aggregate_tokens_spent.saturating_add(tokens_spent);
            }
            ever_bound |= result.bound;
            if ever_bound {
                workflow_progress::emit_agent_update(
                    &self.exec,
                    ledger,
                    &invocation.cell_id,
                    workflow_progress::WorkflowAgentUpdateParams {
                        node_id: progress_context.node_id,
                        attempt,
                        last_attempt_reason: (attempt > 0)
                            .then_some(WorkflowAgentAttemptReason::UserRetry),
                        token_usage: aggregate_progress.token_usage.clone(),
                        tool_call_count: aggregate_progress.tool_call_count,
                        duration_ms: aggregate_duration_ms,
                    },
                )
                .await;
            }

            // `spawn_agent_attempt` returns only after the child is reaped when cancelled, its
            // worktree guard is closed/retained, and scheduler admission has released its permit.
            let finish = registration.decide_after_cleanup(cancellation_token.is_cancelled());
            let logical_finish = match finish {
                AttemptFinish::UserRetry { next_attempt } => {
                    registration.acknowledge(finish);
                    attempt = next_attempt;
                    continue;
                }
                AttemptFinish::Natural => LogicalAgentFinish::Natural,
                AttemptFinish::UserSkip => LogicalAgentFinish::UserSkip,
                AttemptFinish::RetryLimitReached => LogicalAgentFinish::RetryLimitReached,
                AttemptFinish::RunCancellation => LogicalAgentFinish::RunCancellation,
                AttemptFinish::PersistenceFailure => {
                    return AgentSpawnOutcome::Rejected(
                        WORKFLOW_AGENT_JOURNAL_UNAVAILABLE.to_string(),
                    );
                }
            };
            let finalization = self
                .finalize_logical_agent(
                    invocation.cell_id,
                    progress_context.node_id,
                    attempt,
                    journal,
                    result.execution,
                    aggregate_progress,
                    aggregate_duration_ms,
                    aggregate_tokens_spent,
                    had_metered_attempt,
                    ever_bound,
                    logical_finish,
                )
                .await;
            // Final controls acknowledge only after the single authoritative terminal journal
            // append (or its bounded failure) as well as resource cleanup.
            match finalization.persistence {
                TerminalPersistence::Persisted => registration.acknowledge(finish),
                TerminalPersistence::Failed => registration.acknowledge_persistence_failure(),
            }
            return finalization.outcome;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn finalize_logical_agent(
        &self,
        cell_id: CellId,
        node_id: u64,
        attempt: u32,
        journal: Arc<AgentCallJournalCtx>,
        execution: WorkflowAgentExecution,
        aggregate_progress: WorkflowChildProgress,
        aggregate_duration_ms: u64,
        aggregate_tokens_spent: u64,
        had_metered_attempt: bool,
        ever_bound: bool,
        finish: LogicalAgentFinish,
    ) -> LogicalAgentFinalization {
        let WorkflowAgentExecution {
            outcome,
            cancelled,
            child_thread_id,
            rollout_path,
            ..
        } = execution;
        let (
            mut outcome,
            mut status,
            mut returned_null,
            journal_status,
            journal_control_reason,
            journal_return,
        ) = match finish {
            LogicalAgentFinish::UserSkip => (
                AgentSpawnOutcome::Failed,
                AgentStatus::Shutdown,
                true,
                Some(JournalAgentStatus::Completed),
                Some(JournalAgentControlReason::UserSkip),
                JsonValue::Null,
            ),
            LogicalAgentFinish::RetryLimitReached => (
                AgentSpawnOutcome::Failed,
                AgentStatus::Errored(WORKFLOW_AGENT_RETRY_LIMIT_ERROR.to_string()),
                true,
                Some(JournalAgentStatus::Completed),
                Some(JournalAgentControlReason::RetryLimitReached),
                JsonValue::Null,
            ),
            LogicalAgentFinish::RunCancellation => (
                AgentSpawnOutcome::Failed,
                AgentStatus::Interrupted,
                true,
                None,
                None,
                JsonValue::Null,
            ),
            LogicalAgentFinish::Natural if cancelled => (
                AgentSpawnOutcome::Failed,
                AgentStatus::Interrupted,
                true,
                None,
                None,
                JsonValue::Null,
            ),
            LogicalAgentFinish::Natural => match outcome {
                AgentSpawnOutcome::Completed(value) => {
                    let returned_null = value.is_null();
                    (
                        AgentSpawnOutcome::Completed(value.clone()),
                        AgentStatus::Completed(None),
                        returned_null,
                        Some(JournalAgentStatus::Completed),
                        None,
                        value,
                    )
                }
                AgentSpawnOutcome::Failed => (
                    AgentSpawnOutcome::Failed,
                    AgentStatus::Errored("workflow agent returned null".to_string()),
                    true,
                    None,
                    None,
                    JsonValue::Null,
                ),
                AgentSpawnOutcome::Rejected(reason) => (
                    AgentSpawnOutcome::Rejected(reason.clone()),
                    AgentStatus::Errored(reason),
                    false,
                    Some(JournalAgentStatus::Error),
                    None,
                    JsonValue::Null,
                ),
            },
        };
        let journal_tokens = match journal_status {
            Some(JournalAgentStatus::Completed) => Some(aggregate_tokens_spent),
            Some(JournalAgentStatus::Error) => {
                had_metered_attempt.then_some(aggregate_tokens_spent)
            }
            None => had_metered_attempt.then_some(aggregate_tokens_spent),
        };
        let persistence = if let Err(reason) = journal
            .record(AgentCallRecord {
                attempt,
                status: journal_status,
                control_reason: journal_control_reason,
                ret: journal_return,
                child_thread_id,
                rollout_path,
                tokens_spent: journal_tokens,
                progress: Some(journal_progress(&aggregate_progress, aggregate_duration_ms)),
            })
            .await
        {
            outcome = AgentSpawnOutcome::Rejected(reason);
            status = AgentStatus::Errored(WORKFLOW_AGENT_JOURNAL_UNAVAILABLE.to_string());
            returned_null = false;
            TerminalPersistence::Failed
        } else {
            TerminalPersistence::Persisted
        };

        if ever_bound {
            let ledger = self
                .exec
                .session
                .services
                .code_mode_service
                .workflow_run_ledger();
            workflow_progress::emit_agent_end(
                &self.exec,
                ledger,
                &cell_id,
                node_id,
                attempt,
                match finish {
                    LogicalAgentFinish::UserSkip => Some(WorkflowAgentAttemptReason::UserSkip),
                    LogicalAgentFinish::RetryLimitReached => {
                        Some(WorkflowAgentAttemptReason::RetryLimitReached)
                    }
                    LogicalAgentFinish::Natural if attempt > 0 => {
                        Some(WorkflowAgentAttemptReason::UserRetry)
                    }
                    LogicalAgentFinish::Natural | LogicalAgentFinish::RunCancellation => None,
                },
                status,
                aggregate_progress.token_usage,
                aggregate_progress.tool_call_count,
                aggregate_duration_ms,
                returned_null,
            )
            .await;
        }
        LogicalAgentFinalization {
            outcome,
            persistence,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogicalAgentFinish {
    Natural,
    UserSkip,
    RetryLimitReached,
    RunCancellation,
}

struct LogicalAgentFinalization {
    outcome: AgentSpawnOutcome,
    persistence: TerminalPersistence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalPersistence {
    Persisted,
    Failed,
}
