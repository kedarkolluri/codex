use std::sync::Arc;
use std::sync::Weak;
use std::time::Instant;

use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
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
use crate::agent::control::AgentExecutionAdmission;
use crate::agent::control::AgentExecutionCapacityWaiter;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::RunningTask;
use crate::state::TurnState;
use crate::state::turn_lifecycle::TurnGeneration;
use crate::state::turn_lifecycle::TurnStartDriver;
use codex_otel::TURN_E2E_DURATION_METRIC;

pub(crate) enum TaskStartOutcome {
    Started,
    Busy,
    AtCapacity(AgentExecutionCapacityWaiter),
    Cancelled(TurnAbortReason),
    Poisoned,
}

enum TaskStartReservation {
    Fresh,
    Reserved(Arc<tokio::sync::Mutex<TurnState>>),
}

#[must_use = "the exact start driver must be committed or compensated"]
struct PendingTaskStart {
    session: Weak<Session>,
    turn_context: Arc<TurnContext>,
    driver: Option<TurnStartDriver>,
    started_contributors: usize,
}

impl PendingTaskStart {
    fn new(
        session: &Arc<Session>,
        turn_context: Arc<TurnContext>,
        driver: TurnStartDriver,
    ) -> Self {
        Self {
            session: Arc::downgrade(session),
            turn_context,
            driver: Some(driver),
            started_contributors: 0,
        }
    }

    fn generation(&self) -> TurnGeneration {
        self.driver
            .as_ref()
            .expect("pending start must retain its driver")
            .generation()
    }

    fn take_driver(&mut self) -> TurnStartDriver {
        self.driver
            .take()
            .expect("pending start must retain its driver")
    }

    fn restore_driver(&mut self, driver: TurnStartDriver) {
        assert!(self.driver.replace(driver).is_none());
    }

    async fn compensate(mut self) -> TaskStartOutcome {
        let Some(session) = self.session.upgrade() else {
            self.driver.take();
            return TaskStartOutcome::Cancelled(TurnAbortReason::Interrupted);
        };
        let reason = self
            .generation()
            .cancel_reason()
            .unwrap_or(TurnAbortReason::Interrupted);
        if self.started_contributors > 0 {
            session
                .emit_started_turn_abort_lifecycle(
                    reason.clone(),
                    self.turn_context.extension_data.as_ref(),
                    self.started_contributors,
                )
                .await;
            self.started_contributors = 0;
        }
        let mut active_turn = session.active_turn.lock().await;
        let driver = self.take_driver();
        let result = active_turn.complete_cancelled_start(driver);
        drop(active_turn);
        match result {
            Ok(reason) => TaskStartOutcome::Cancelled(reason),
            Err(driver) => {
                self.restore_driver(driver);
                self.poison().await
            }
        }
    }

    async fn poison(mut self) -> TaskStartOutcome {
        let generation = self.generation();
        let Some(session) = self.session.upgrade() else {
            self.driver.take();
            return TaskStartOutcome::Poisoned;
        };
        let cancelled_exact = {
            let mut active_turn = session.active_turn.lock().await;
            active_turn.cancel_start_exact(&generation, TurnAbortReason::Interrupted)
        };
        if !cancelled_exact {
            self.driver.take();
            return TaskStartOutcome::Poisoned;
        }
        if self.started_contributors > 0 {
            let reason = generation
                .cancel_reason()
                .unwrap_or(TurnAbortReason::Interrupted);
            session
                .emit_started_turn_abort_lifecycle(
                    reason,
                    self.turn_context.extension_data.as_ref(),
                    self.started_contributors,
                )
                .await;
            self.started_contributors = 0;
        }
        let mut active_turn = session.active_turn.lock().await;
        let driver = self.take_driver();
        drop(driver);
        active_turn.poison_abandoned_start(&generation);
        TaskStartOutcome::Poisoned
    }
}

impl Drop for PendingTaskStart {
    fn drop(&mut self) {
        let Some(driver) = self.driver.take() else {
            return;
        };
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let turn_context = Arc::clone(&self.turn_context);
        let started_contributors = self.started_contributors;
        let generation = driver.generation();
        let runtime = session.services.runtime_handle.clone();
        runtime.spawn(async move {
            let cancelled_exact = session
                .active_turn
                .lock()
                .await
                .cancel_start_exact(&generation, TurnAbortReason::Interrupted);
            if !cancelled_exact {
                return;
            }
            let reason = generation
                .cancel_reason()
                .unwrap_or(TurnAbortReason::Interrupted);
            if started_contributors > 0 {
                session
                    .emit_started_turn_abort_lifecycle(
                        reason,
                        turn_context.extension_data.as_ref(),
                        started_contributors,
                    )
                    .await;
            }
            let compensation = {
                let mut active_turn = session.active_turn.lock().await;
                active_turn.complete_cancelled_start(driver)
            };
            match compensation {
                Ok(_) => {}
                Err(driver) => {
                    drop(driver);
                    session
                        .active_turn
                        .lock()
                        .await
                        .poison_abandoned_start(&generation);
                }
            }
        });
    }
}

impl Session {
    pub async fn spawn_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) {
        self.abort_all_tasks(TurnAbortReason::Replaced).await;
        self.clear_connector_selection().await;
        match self
            .start_task(Arc::clone(&turn_context), input, task)
            .await
        {
            TaskStartOutcome::Started => {}
            TaskStartOutcome::Cancelled(reason) => {
                tracing::trace!(?reason, "task start cancelled before commit");
            }
            TaskStartOutcome::AtCapacity(waiter) => {
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Error(
                        waiter
                            .into_limit_error()
                            .to_error_event(/*message_prefix*/ None),
                    ),
                )
                .await;
            }
            TaskStartOutcome::Busy => {
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Error(ErrorEvent {
                        message: "turn could not start because the session task slot is busy"
                            .to_string(),
                        codex_error_info: Some(CodexErrorInfo::BadRequest),
                    }),
                )
                .await;
            }
            TaskStartOutcome::Poisoned => {
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Warning(WarningEvent {
                        message: "turn start failed closed after an internal lifecycle mismatch"
                            .to_string(),
                    }),
                )
                .await;
            }
        }
    }

    pub(crate) async fn start_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) -> TaskStartOutcome {
        self.start_task_with_reservation(
            turn_context,
            input,
            task,
            TaskStartReservation::Fresh,
        )
        .await
    }

    pub(crate) async fn start_reserved_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
        reserved_turn_state: Arc<tokio::sync::Mutex<TurnState>>,
    ) -> TaskStartOutcome {
        self.start_task_with_reservation(
            turn_context,
            input,
            task,
            TaskStartReservation::Reserved(reserved_turn_state),
        )
        .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "mailbox attachment and task commit must remain atomic under the turn slot lock"
    )]
    async fn start_task_with_reservation<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
        reservation: TaskStartReservation,
    ) -> TaskStartOutcome {
        let driver = {
            let mut active_turn = self.active_turn.lock().await;
            let can_begin = match &reservation {
                TaskStartReservation::Fresh => active_turn.can_begin_fresh_start(),
                TaskStartReservation::Reserved(turn_state) => {
                    active_turn.can_begin_reserved_start(turn_state)
                }
            };
            if !can_begin {
                return TaskStartOutcome::Busy;
            }
            let lease = match self.services.agent_control.execution_admission(
                turn_context.multi_agent_version,
                &turn_context.session_source,
            ) {
                AgentExecutionAdmission::Unrestricted => None,
                AgentExecutionAdmission::Admitted(guard) => Some(guard),
                AgentExecutionAdmission::AtCapacity(waiter) => {
                    if let TaskStartReservation::Reserved(turn_state) = &reservation {
                        active_turn.clear_taskless_exact_state(turn_state);
                    }
                    return TaskStartOutcome::AtCapacity(waiter);
                }
            };
            let driver = match &reservation {
                TaskStartReservation::Fresh => active_turn.begin_fresh_start(lease),
                TaskStartReservation::Reserved(turn_state) => {
                    active_turn.begin_reserved_start(turn_state, lease)
                }
            };
            let Ok(driver) = driver else {
                return TaskStartOutcome::Busy;
            };
            driver
        };
        let mut pending_start =
            PendingTaskStart::new(self, Arc::clone(&turn_context), driver);
        let generation = pending_start.generation();

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

        if generation.cancel_reason().is_some() {
            return pending_start.compensate().await;
        }
        self.emit_turn_start_lifecycle(
            turn_context.as_ref(),
            &token_usage_at_turn_start,
            &generation,
            &mut pending_start.started_contributors,
        )
        .await;
        if generation.cancel_reason().is_some() {
            return pending_start.compensate().await;
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
        let task_input = input;
        let task_cancellation_token = cancellation_token.child_token();
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
        if generation.cancel_reason().is_some() {
            drop(active_turn);
            return pending_start.compensate().await;
        }
        generation.turn_state().lock().await.token_usage_at_turn_start =
            token_usage_at_turn_start;
        self.input_queue
            .attach_mailbox_input_to_starting_turn(generation.turn_state().as_ref())
            .await;

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
                    sess.on_task_finished(
                        generation_for_finish,
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
            turn_context,
            turn_extension_data,
            _timer: timer,
        };
        let driver = pending_start.take_driver();
        match active_turn.commit_start(driver, running_task) {
            Ok(()) => {
                assert!(
                    commit_tx.send(()).is_ok(),
                    "newly spawned task must retain its commit gate"
                );
                drop(active_turn);
                TaskStartOutcome::Started
            }
            Err((driver, running_task)) => {
                pending_start.restore_driver(driver);
                drop(running_task);
                drop(commit_tx);
                drop(active_turn);
                if generation.cancel_reason().is_some() {
                    pending_start.compensate().await
                } else {
                    pending_start.poison().await
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "start_tests.rs"]
mod tests;
