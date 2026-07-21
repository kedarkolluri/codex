use std::sync::Arc;
use std::time::Instant;

use codex_otel::TURN_E2E_DURATION_METRIC;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::WarningEvent;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::field;
use tracing::info_span;
use tracing::trace_span;
use tracing::warn;

use super::AnySessionTask;
use super::SessionTask;
use super::SessionTaskContext;
use super::finalization::PendingFinalization;
use super::pending_start::PendingTaskStart;
use super::pending_start::PendingTaskStartOutcome;
use crate::agent::control::AgentExecutionAdmission;
use crate::agent::control::AgentExecutionCapacityWaiter;
use crate::session::AutomaticTurnStartTicket;
use crate::session::TurnInput;
use crate::session::TurnStartAdmissionPermit;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::RunningTask;
use crate::state::turn_lifecycle::TurnGeneration;

pub(crate) enum TaskStartOutcome {
    Started,
    Busy,
    StartInProgress(TurnGeneration),
    PendingTriggerTurn,
    AtCapacity(AgentExecutionCapacityWaiter),
    Cancelled(TurnAbortReason),
    Poisoned,
}

#[derive(Clone, Copy)]
enum TaskStartAdmission {
    Unconditional,
    AutomaticIdle(AutomaticTurnStartTicket),
    AutomaticTrigger(AutomaticTurnStartTicket),
}

impl TaskStartAdmission {
    fn is_open(self, session: &Session) -> bool {
        match self {
            Self::Unconditional => session.turn_start_gate.is_open(),
            Self::AutomaticIdle(ticket) | Self::AutomaticTrigger(ticket) => {
                session.turn_start_gate.admits_automatic_start(ticket)
            }
        }
    }

    fn ticket_for_finish(self, session: &Session) -> Option<AutomaticTurnStartTicket> {
        match self {
            Self::Unconditional => session.turn_start_gate.automatic_start_ticket(),
            Self::AutomaticIdle(ticket) | Self::AutomaticTrigger(ticket) => Some(ticket),
        }
    }
}

struct TaskStartInput {
    task: Vec<TurnInput>,
    pending: Vec<TurnInput>,
}

impl From<PendingTaskStartOutcome> for TaskStartOutcome {
    fn from(outcome: PendingTaskStartOutcome) -> Self {
        match outcome {
            PendingTaskStartOutcome::Cancelled(reason) => Self::Cancelled(reason),
            PendingTaskStartOutcome::Poisoned => Self::Poisoned,
        }
    }
}

impl Session {
    pub(super) async fn start_task_with_admission_permit<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
        admission_permit: TurnStartAdmissionPermit,
    ) -> TaskStartOutcome {
        self.start_task_inner(
            turn_context,
            TaskStartInput {
                task: input,
                pending: Vec::new(),
            },
            task,
            TaskStartAdmission::Unconditional,
            admission_permit,
        )
        .await
    }

    pub(super) async fn start_automatic_trigger_task<T: SessionTask>(
        self: &Arc<Self>,
        ticket: AutomaticTurnStartTicket,
        turn_context: Arc<TurnContext>,
        task: T,
        admission_permit: TurnStartAdmissionPermit,
    ) -> TaskStartOutcome {
        self.start_task_inner(
            turn_context,
            TaskStartInput {
                task: Vec::new(),
                pending: Vec::new(),
            },
            task,
            TaskStartAdmission::AutomaticTrigger(ticket),
            admission_permit,
        )
        .await
    }

    pub(crate) async fn start_automatic_task_with_pending_input<T: SessionTask>(
        self: &Arc<Self>,
        ticket: AutomaticTurnStartTicket,
        turn_context: Arc<TurnContext>,
        pending_input: Vec<TurnInput>,
        task: T,
        admission_permit: TurnStartAdmissionPermit,
    ) -> TaskStartOutcome {
        self.start_task_inner(
            turn_context,
            TaskStartInput {
                task: Vec::new(),
                pending: pending_input,
            },
            task,
            TaskStartAdmission::AutomaticIdle(ticket),
            admission_permit,
        )
        .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "mailbox attachment and task commit must remain atomic under the turn slot lock"
    )]
    async fn start_task_inner<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: TaskStartInput,
        task: T,
        admission: TaskStartAdmission,
        admission_permit: TurnStartAdmissionPermit,
    ) -> TaskStartOutcome {
        let (driver, automatic_start_ticket_for_finish) = {
            let mut active_turn = self.active_turn.lock().await;
            if !active_turn.can_begin_fresh_start() {
                return active_turn
                    .starting_generation()
                    .map_or(TaskStartOutcome::Busy, TaskStartOutcome::StartInProgress);
            }
            if !admission.is_open(self) {
                return TaskStartOutcome::Cancelled(TurnAbortReason::Interrupted);
            }
            match admission {
                TaskStartAdmission::Unconditional => {}
                TaskStartAdmission::AutomaticIdle(_) => {
                    if self.input_queue.has_trigger_turn_mailbox_items().await {
                        return TaskStartOutcome::PendingTriggerTurn;
                    }
                }
                TaskStartAdmission::AutomaticTrigger(_) => {
                    if !self.input_queue.has_trigger_turn_mailbox_items().await {
                        return TaskStartOutcome::Busy;
                    }
                }
            }
            let lease = match self.services.agent_control.execution_admission(
                turn_context.multi_agent_version,
                &turn_context.session_source,
            ) {
                AgentExecutionAdmission::Unrestricted => None,
                AgentExecutionAdmission::Admitted(guard) => Some(guard),
                AgentExecutionAdmission::AtCapacity(waiter) => {
                    return TaskStartOutcome::AtCapacity(waiter);
                }
            };
            let Ok(driver) = active_turn.begin_fresh_start(lease) else {
                return active_turn
                    .starting_generation()
                    .map_or(TaskStartOutcome::Busy, TaskStartOutcome::StartInProgress);
            };
            (driver, admission.ticket_for_finish(self))
        };
        let mut pending_start =
            PendingTaskStart::new(Arc::clone(self), Arc::clone(&turn_context), driver);
        drop(admission_permit);
        let generation = pending_start.generation();
        let TaskStartInput {
            task: task_input,
            pending: pending_input,
        } = input;

        let task: Arc<dyn AnySessionTask> = Arc::new(task);
        let task_kind = task.kind();
        let span_name = task.span_name();
        let started_at = Instant::now();
        let turn_started_at_unix_ms = turn_context
            .turn_timing_state
            .mark_turn_started(started_at)
            .await;
        turn_context
            .turn_metadata_state
            .set_turn_started_at_unix_ms(turn_started_at_unix_ms);
        let token_usage_at_turn_start = self.total_token_usage().await.unwrap_or_default();

        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);

        if !admission.is_open(self) {
            self.active_turn
                .lock()
                .await
                .cancel_start_exact(&generation, TurnAbortReason::Interrupted);
        }
        if generation.cancel_reason().is_some() {
            return pending_start.compensate().await.into();
        }
        self.emit_cancellable_turn_start_lifecycle(
            turn_context.as_ref(),
            &token_usage_at_turn_start,
            &generation,
            pending_start.lifecycle_progress_mut(),
        )
        .await;
        if generation.cancel_reason().is_some() {
            return pending_start.compensate().await.into();
        }

        let cancellation_token = CancellationToken::new();
        let done = Arc::new(Notify::new());
        let turn_extension_data = Arc::clone(&turn_context.extension_data);
        let done_clone = Arc::clone(&done);
        let session_ctx = Arc::new(SessionTaskContext::new(
            Arc::clone(self),
            Arc::clone(&turn_extension_data),
        ));
        let ctx = Arc::clone(&turn_context);
        let task_for_run = Arc::clone(&task);
        let task_cancellation_token = cancellation_token.child_token();
        // Task-owned turn spans keep a core-owned span open for the
        // full task lifecycle after the submission dispatch span ends.
        let reasoning_effort = turn_context.effective_reasoning_effort_for_tracing();
        let task_span = info_span!(
            "turn",
            otel.name = span_name,
            thread.id = %self.thread_id,
            turn.id = %turn_context.sub_id,
            model = %turn_context.model_info.slug,
            codex.turn.reasoning_effort = %reasoning_effort,
            codex.turn.token_usage.input_tokens = field::Empty,
            codex.turn.token_usage.cached_input_tokens = field::Empty,
            codex.turn.token_usage.cache_write_input_tokens = field::Empty,
            codex.turn.token_usage.non_cached_input_tokens = field::Empty,
            codex.turn.token_usage.output_tokens = field::Empty,
            codex.turn.token_usage.reasoning_output_tokens = field::Empty,
            codex.turn.token_usage.total_tokens = field::Empty,
        );
        let (commit_tx, commit_rx) = oneshot::channel();
        let generation_for_finish = generation.clone();

        let mut active_turn = self.active_turn.lock().await;
        if !admission.is_open(self) {
            active_turn.cancel_start_exact(&generation, TurnAbortReason::Interrupted);
        }
        if generation.cancel_reason().is_some() {
            drop(active_turn);
            return pending_start.compensate().await.into();
        }
        generation
            .turn_state()
            .lock()
            .await
            .token_usage_at_turn_start = token_usage_at_turn_start;
        let prepared_input = self
            .input_queue
            .prepare_starting_turn_input(generation.turn_state().as_ref(), pending_input)
            .await;
        if matches!(admission, TaskStartAdmission::AutomaticIdle(_))
            && prepared_input.has_trigger_turn_mailbox_items()
        {
            active_turn.cancel_start_exact(&generation, TurnAbortReason::Replaced);
            drop(prepared_input);
            drop(active_turn);
            return match pending_start.compensate().await {
                PendingTaskStartOutcome::Cancelled(_) => TaskStartOutcome::PendingTriggerTurn,
                PendingTaskStartOutcome::Poisoned => TaskStartOutcome::Poisoned,
            };
        }

        let handle = tokio::spawn(
            async move {
                let Ok(()) = commit_rx.await else {
                    done_clone.notify_waiters();
                    return;
                };
                let ctx_for_finish = Arc::clone(&ctx);
                let task_result = task_for_run
                    .run(
                        Arc::clone(&session_ctx),
                        ctx,
                        task_input,
                        task_cancellation_token.child_token(),
                    )
                    .instrument(trace_span!("session_task.run"))
                    .await;
                let sess = session_ctx.clone_session();
                if let Err(err) = sess.flush_rollout().await {
                    warn!("failed to flush rollout before completing turn: {err}");
                    sess.send_event(
                        ctx_for_finish.as_ref(),
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "Failed to save the conversation transcript; Codex will continue retrying. Error: {err}"
                            ),
                        }),
                    )
                    .await;
                }
                if !task_cancellation_token.is_cancelled() {
                    // Finish uniformly from the spawn site so all tasks share the same lifecycle.
                    sess.on_task_finished(
                        generation_for_finish,
                        automatic_start_ticket_for_finish,
                        Arc::clone(&ctx_for_finish),
                        task_result,
                    )
                    .await;
                }
                done_clone.notify_waiters();
            }
            .instrument(task_span),
        );
        let timer = turn_context
            .session_telemetry
            .start_timer(TURN_E2E_DURATION_METRIC, &[])
            .ok();
        let running_task = RunningTask {
            done,
            handle: AbortOnDropHandle::new(handle),
            kind: task_kind,
            task,
            cancellation_token,
            turn_context: Arc::clone(&turn_context),
            turn_extension_data,
            _agent_execution_guard: None,
            _timer: timer,
        };
        match pending_start.commit(&mut active_turn, running_task) {
            Ok(mut lifecycle_progress) => {
                if commit_tx.send(()).is_ok() {
                    prepared_input.commit();
                    drop(active_turn);
                    return TaskStartOutcome::Started;
                }
                drop(prepared_input);
                let pending_finalization = if let Some(finalizing_turn) =
                    active_turn.begin_finalization(&generation, &turn_context)
                {
                    let (task, completion) = finalizing_turn.into_parts();
                    drop(task);
                    Some(PendingFinalization::new(Arc::clone(self), completion))
                } else {
                    self.turn_start_gate.close();
                    None
                };
                drop(active_turn);
                self.emit_entered_turn_abort_lifecycle(
                    TurnAbortReason::Interrupted,
                    turn_context.extension_data.as_ref(),
                    &mut lifecycle_progress,
                )
                .await;
                if let Some(pending_finalization) = pending_finalization {
                    pending_finalization.poison().await;
                }
                TaskStartOutcome::Poisoned
            }
            Err((pending_start, running_task)) => {
                drop(running_task);
                drop(commit_tx);
                drop(prepared_input);
                drop(active_turn);
                pending_start.poison().await.into()
            }
        }
    }
}
